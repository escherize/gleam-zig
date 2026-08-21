// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 The Gleam contributors

// Gleam Zig target runtime prelude.
//
// Values use a uniform tagged-union representation, mirroring the dynamic
// representation of the JavaScript target. Memory is reference counted
// (Perceus: naive counting plus compiler-inserted last-use moves and
// cons-cell reuse; borrowing inference is future work).
//
// Ownership protocol (matches the code generator):
// - Every helper that takes Value arguments CONSUMES them (takes over one
//   reference) unless marked "borrows" — pattern-test helpers borrow.
// - Every helper returns an OWNED value.
// - Generated code dups a variable at each use and drops every binding
//   when its scope exits.
// - FFI functions receive borrowed values and return owned values; the
//   generated forwarding functions bridge the conventions.
//
// Int is i64 with wrapping arithmetic (the JavaScript target uses f64
// numbers, Erlang has bignums; targets choose a pragmatic representation).

const std = @import("std");
const builtin = @import("builtin");

/// Debug builds (the `gleam run` default via `zig run`) use a
/// leak-checking allocator and verify on exit that no reference-counted
/// allocation outlives main. Release builds use the fast allocator and
/// skip the check.
const leak_checking = builtin.mode == .Debug;

pub var debug_allocator: std.heap.DebugAllocator(.{}) = .init;

/// Command-line arguments, stashed by the generated entrypoint for the
/// argv FFI.
pub var process_args: std.process.Args = undefined;

fn rc_allocator() std.mem.Allocator {
    return if (leak_checking) debug_allocator.allocator() else std.heap.smp_allocator;
}

/// Scratch allocator for FFI temporaries that are not reference counted.
pub const allocator = std.heap.page_allocator;

pub const Value = union(enum) {
    int: i64,
    float: f64,
    bool: bool,
    /// Always an owned, reference-counted buffer (or empty). Literals and
    /// slices are copied on construction; buffer sharing is a later
    /// optimisation.
    string: []const u8,
    nil,
    /// Linked list; null is the empty list.
    list: ?*const Cons,
    tuple: []const Value,
    /// Custom type value. Variants are identified by name.
    record: *const Record,
    closure: Closure,
    /// Byte-aligned bit array; views share a reference-counted buffer.
    bit_array: *const BitArray,
};

pub const BitArray = struct {
    rc: usize,
    /// The full shared buffer (an rc'd string-style allocation, or empty).
    buffer: []const u8,
    offset: usize,
    length: usize,

    pub fn bytes(self: *const BitArray) []const u8 {
        return self.buffer[self.offset .. self.offset + self.length];
    }
};

pub const Cons = struct {
    rc: usize,
    head: Value,
    tail: ?*const Cons,
};

pub const Record = struct {
    rc: usize,
    /// Variant name, e.g. "Ok". Variant identity is (name, arity), which is
    /// unique within a type, and values of different types never meet in a
    /// well-typed pattern match. Always a static string.
    name: []const u8,
    fields: []const Value,
    /// Field labels for inspection, null when a field is positional.
    /// Empty when no field has a label. Always static strings.
    labels: []const ?[]const u8 = &.{},
};

/// All function values share one shape: a type-erased pointer to a lifted
/// function whose first parameter is the captured environment. Call sites
/// know the arity statically and cast through callN below. The closure's
/// reference count lives on its env allocation; capture-free closures own
/// no heap at all.
pub const Closure = struct {
    function: *const anyopaque,
    env: []const Value,
};

// ---------------------------------------------------------------- rc core

/// Value slices (tuples, closure envs) and string buffers are allocated
/// with one extra leading word holding the reference count.

// Release-mode object pools. The dominant cost in hot numeric code is
// allocator round-trips for tiny objects (a record + its field slice per
// vector operation). Freed records, cons cells and small slices park in
// threadlocal free lists for immediate reuse. Compiled out in Debug so
// the leak-checking gate keeps exact alloc/free pairing.
const pooling = !leak_checking;
const max_pooled_slice = 8;

threadlocal var record_pool: ?*Record = null;
threadlocal var cons_pool: ?*Cons = null;
threadlocal var slice_pools: [max_pooled_slice + 1]?[*]Value =
    @splat(null);
// String buffers pooled by word count (payload up to 56 bytes).
const max_pooled_string_words = 8;
threadlocal var string_pools: [max_pooled_string_words + 1]?[*]u64 =
    @splat(null);

fn poolPopString(words: usize) ?[]u64 {
    if (!pooling or words < 2 or words > max_pooled_string_words) return null;
    const base = string_pools[words] orelse return null;
    const next = base[1];
    string_pools[words] = if (next == 0) null else @ptrFromInt(next);
    base[0] = 1;
    return base[0..words];
}

fn poolPushString(base: [*]u64, words: usize) bool {
    if (!pooling or words < 2 or words > max_pooled_string_words) return false;
    base[1] = if (string_pools[words]) |head| @intFromPtr(head) else 0;
    string_pools[words] = base;
    return true;
}

fn poolPopRecord() ?*Record {
    if (!pooling) return null;
    const head = record_pool orelse return null;
    // The next pointer hides in the retired struct's name field.
    record_pool = @ptrFromInt(@intFromPtr(head.name.ptr));
    if (head.name.len == 0) record_pool = null;
    return head;
}

fn poolPushRecord(record: *Record) bool {
    if (!pooling) return false;
    record.name = if (record_pool) |next|
        @as([*]const u8, @ptrCast(next))[0..1]
    else
        &.{};
    record_pool = record;
    return true;
}

fn poolPopCons() ?*Cons {
    if (!pooling) return null;
    const head = cons_pool orelse return null;
    cons_pool = @constCast(head.tail);
    return head;
}

fn poolPushCons(cell: *Cons) bool {
    if (!pooling) return false;
    cell.tail = cons_pool;
    cons_pool = cell;
    return true;
}

fn poolPopSlice(count: usize) ?[]Value {
    if (!pooling or count == 0 or count > max_pooled_slice) return null;
    const base = slice_pools[count] orelse return null;
    // The next pointer hides in the first payload word.
    const next: usize = @bitCast(base[1].int);
    slice_pools[count] = if (next == 0) null else @ptrFromInt(next);
    base[0] = Value{ .int = 1 };
    return base[1 .. count + 1];
}

fn poolPushSlice(payload: []const Value) bool {
    if (!pooling or payload.len == 0 or payload.len > max_pooled_slice) return false;
    const base: [*]Value = @constCast(payload.ptr) - 1;
    const next: usize = if (slice_pools[payload.len]) |head|
        @intFromPtr(head)
    else
        0;
    base[1] = Value{ .int = @bitCast(next) };
    slice_pools[payload.len] = base;
    return true;
}

fn allocValueSlice(count: usize) []Value {
    if (poolPopSlice(count)) |recycled| return recycled;
    const words = rc_allocator().alloc(Value, count + 1) catch @panic("out of memory");
    words[0] = Value{ .int = 1 };
    return words[1..];
}

fn valueSliceRc(payload: []const Value) *i64 {
    const base: [*]Value = @constCast(payload.ptr) - 1;
    return &base[0].int;
}

fn freeValueSlice(payload: []const Value) void {
    if (poolPushSlice(payload)) return;
    const base: [*]Value = @constCast(payload.ptr) - 1;
    rc_allocator().free(base[0 .. payload.len + 1]);
}

fn stringWordCount(byte_length: usize) usize {
    return 1 + (byte_length + 7) / 8;
}

fn allocString(byte_length: usize) []u8 {
    const word_count = stringWordCount(byte_length);
    if (poolPopString(word_count)) |recycled| {
        return std.mem.sliceAsBytes(recycled[1..])[0..byte_length];
    }
    const words = rc_allocator().alloc(u64, word_count) catch
        @panic("out of memory");
    words[0] = 1;
    return std.mem.sliceAsBytes(words[1..])[0..byte_length];
}

fn stringRc(payload: []const u8) *u64 {
    const base: [*]u64 = @alignCast(@as([*]u64, @ptrFromInt(@intFromPtr(payload.ptr))) - 1);
    return &base[0];
}

fn freeString(payload: []const u8) void {
    const base: [*]u64 = @alignCast(@as([*]u64, @ptrFromInt(@intFromPtr(payload.ptr))) - 1);
    const word_count = stringWordCount(payload.len);
    if (poolPushString(base, word_count)) return;
    rc_allocator().free(base[0..word_count]);
}

/// Take an extra reference to a value. No-op for unboxed values.
pub fn dup(value: Value) Value {
    switch (value) {
        .int, .float, .bool, .nil => {},
        .string => |s| if (s.len != 0) {
            stringRc(s).* += 1;
        },
        .list => |cell| if (cell) |c| {
            @constCast(c).rc += 1;
        },
        .tuple => |t| if (t.len != 0) {
            valueSliceRc(t).* += 1;
        },
        .record => |r| {
            @constCast(r).rc += 1;
        },
        .closure => |c| if (c.env.len != 0) {
            valueSliceRc(c.env).* += 1;
        },
        .bit_array => |b| {
            @constCast(b).rc += 1;
        },
    }
    return value;
}

/// Release one reference, freeing (recursively) on reaching zero. The list
/// spine is freed iteratively so long lists cannot overflow the stack.
pub fn drop(value: Value) void {
    switch (value) {
        .int, .float, .bool, .nil => {},
        .string => |s| if (s.len != 0) {
            const rc = stringRc(s);
            rc.* -= 1;
            if (rc.* == 0) freeString(s);
        },
        .list => |cell| dropList(cell),
        .tuple => |t| if (t.len != 0) {
            const rc = valueSliceRc(t);
            rc.* -= 1;
            if (rc.* == 0) {
                for (t) |element| drop(element);
                freeValueSlice(t);
            }
        },
        .record => |r| {
            const mutable = @constCast(r);
            mutable.rc -= 1;
            if (mutable.rc == 0) {
                for (r.fields) |field| drop(field);
                if (r.fields.len != 0) freeValueSlice(r.fields);
                if (!poolPushRecord(mutable)) rc_allocator().destroy(mutable);
            }
        },
        .closure => |c| if (c.env.len != 0) {
            const rc = valueSliceRc(c.env);
            rc.* -= 1;
            if (rc.* == 0) {
                for (c.env) |element| drop(element);
                freeValueSlice(c.env);
            }
        },
        .bit_array => |b| {
            const mutable = @constCast(b);
            mutable.rc -= 1;
            if (mutable.rc == 0) {
                if (b.buffer.len != 0) {
                    const rc = stringRc(b.buffer);
                    rc.* -= 1;
                    if (rc.* == 0) freeString(b.buffer);
                }
                rc_allocator().destroy(mutable);
            }
        },
    }
}

fn dropList(head: ?*const Cons) void {
    var cell = head;
    while (cell) |c| {
        const mutable = @constCast(c);
        mutable.rc -= 1;
        if (mutable.rc != 0) return;
        drop(c.head);
        const next = c.tail;
        if (!poolPushCons(mutable)) rc_allocator().destroy(mutable);
        cell = next;
    }
}

/// Report leaked reference-counted allocations after main returns.
/// A no-op in release builds.
pub fn leakCheckExit() void {
    if (!leak_checking) return;
    const leaks = debug_allocator.detectLeaks();
    if (leaks != 0) {
        std.debug.print("gleam-zig: {d} leaked allocation(s)\n", .{leaks});
        std.process.exit(2);
    }
}

// ------------------------------------------------------------ construction

pub fn intValue(i: i64) Value {
    return Value{ .int = i };
}

pub fn floatValue(f: f64) Value {
    return Value{ .float = f };
}

pub fn boolValue(b: bool) Value {
    return Value{ .bool = b };
}

/// Copy bytes (a literal, an FFI scratch buffer, or a slice of another
/// string) into an owned reference-counted string.
pub fn copyString(bytes: []const u8) Value {
    if (bytes.len == 0) return Value{ .string = &.{} };
    const owned = allocString(bytes.len);
    @memcpy(owned, bytes);
    return Value{ .string = owned };
}

pub const NIL = Value{ .nil = {} };
pub const TRUE = Value{ .bool = true };
pub const FALSE = Value{ .bool = false };

pub fn emptyList() Value {
    return Value{ .list = null };
}

/// Borrows: wraps a spine pointer for pattern bindings; the code generator
/// dups the result.
pub fn listValue(cell: ?*const Cons) Value {
    return Value{ .list = cell };
}

/// Consumes head and tail.
pub fn cons(head: Value, tail: Value) Value {
    const cell = poolPopCons() orelse
        rc_allocator().create(Cons) catch @panic("out of memory");
    cell.* = Cons{ .rc = 1, .head = head, .tail = tail.list };
    return Value{ .list = cell };
}

/// Consumes the elements and the tail; the slice itself is not kept.
pub fn listFromSlice(elements: []const Value, tail: Value) Value {
    var result = tail;
    var index = elements.len;
    while (index > 0) {
        index -= 1;
        result = cons(elements[index], result);
    }
    return result;
}

/// Consumes the elements; the slice itself is not kept.
pub fn tupleValue(elements: []const Value) Value {
    if (elements.len == 0) return Value{ .tuple = &.{} };
    const owned = allocValueSlice(elements.len);
    @memcpy(owned, elements);
    return Value{ .tuple = owned };
}

/// Consumes the fields; name must be a static string.
pub fn makeRecord(name: []const u8, fields: []const Value) Value {
    return makeRecordL(name, fields, &.{});
}

/// Consumes the fields; name and labels must be static strings.
pub fn makeRecordL(
    name: []const u8,
    fields: []const Value,
    labels: []const ?[]const u8,
) Value {
    const record = poolPopRecord() orelse
        rc_allocator().create(Record) catch @panic("out of memory");
    var owned_fields: []const Value = &.{};
    if (fields.len != 0) {
        const copied = allocValueSlice(fields.len);
        @memcpy(copied, fields);
        owned_fields = copied;
    }
    record.* = Record{ .rc = 1, .name = name, .fields = owned_fields, .labels = labels };
    return Value{ .record = record };
}

/// Consumes the environment values; the slice itself is not kept.
pub fn makeClosure(function: *const anyopaque, env: []const Value) Value {
    if (env.len == 0) {
        return Value{ .closure = Closure{ .function = function, .env = &.{} } };
    }
    const owned = allocValueSlice(env.len);
    @memcpy(owned, env);
    return Value{ .closure = Closure{ .function = function, .env = owned } };
}

// ------------------------------------------------------- field extraction

/// Consumes the record, returns the owned field value.
pub fn recordField(record: Value, index: usize) Value {
    const field = dup(record.record.fields[index]);
    drop(record);
    return field;
}

/// Consumes the tuple, returns the owned element.
pub fn tupleField(tuple: Value, index: usize) Value {
    const element = dup(tuple.tuple[index]);
    drop(tuple);
    return element;
}

// -------------------------------------------------------------- int maths
// Wrapping, matching the no-overflow-panic semantics of the other targets
// (which never overflow). Unboxed: nothing to consume.

pub fn addInt(a: Value, b: Value) Value {
    return intValue(a.int +% b.int);
}

pub fn subInt(a: Value, b: Value) Value {
    return intValue(a.int -% b.int);
}

pub fn multInt(a: Value, b: Value) Value {
    return intValue(a.int *% b.int);
}

// Division by zero is zero in Gleam.
pub fn divInt(a: Value, b: Value) Value {
    if (b.int == 0) return intValue(0);
    return intValue(@divTrunc(a.int, b.int));
}

pub fn remainderInt(a: Value, b: Value) Value {
    if (b.int == 0) return intValue(0);
    return intValue(@rem(a.int, b.int));
}

pub fn addFloat(a: Value, b: Value) Value {
    return floatValue(a.float + b.float);
}

pub fn subFloat(a: Value, b: Value) Value {
    return floatValue(a.float - b.float);
}

pub fn multFloat(a: Value, b: Value) Value {
    return floatValue(a.float * b.float);
}

pub fn divFloat(a: Value, b: Value) Value {
    if (b.float == 0.0) return floatValue(0.0);
    return floatValue(a.float / b.float);
}

pub fn negateInt(a: Value) Value {
    return intValue(0 -% a.int);
}

pub fn negateBool(a: Value) Value {
    return boolValue(!a.bool);
}

pub fn ltInt(a: Value, b: Value) Value {
    return boolValue(a.int < b.int);
}

pub fn ltEqInt(a: Value, b: Value) Value {
    return boolValue(a.int <= b.int);
}

pub fn gtInt(a: Value, b: Value) Value {
    return boolValue(a.int > b.int);
}

pub fn gtEqInt(a: Value, b: Value) Value {
    return boolValue(a.int >= b.int);
}

pub fn ltFloat(a: Value, b: Value) Value {
    return boolValue(a.float < b.float);
}

pub fn ltEqFloat(a: Value, b: Value) Value {
    return boolValue(a.float <= b.float);
}

pub fn gtFloat(a: Value, b: Value) Value {
    return boolValue(a.float > b.float);
}

pub fn gtEqFloat(a: Value, b: Value) Value {
    return boolValue(a.float >= b.float);
}

// ---------------------------------------------------------------- closure
// callN consumes the closure and the arguments (the callee owns its
// parameters; the env is borrowed for the duration of the call).

pub fn call0(f: Value) Value {
    const fp: *const fn ([]const Value) Value = @ptrCast(@alignCast(f.closure.function));
    const result = fp(f.closure.env);
    drop(f);
    return result;
}

pub fn call1(f: Value, a: Value) Value {
    const fp: *const fn ([]const Value, Value) Value = @ptrCast(@alignCast(f.closure.function));
    const result = fp(f.closure.env, a);
    drop(f);
    return result;
}

pub fn call2(f: Value, a: Value, b: Value) Value {
    const fp: *const fn ([]const Value, Value, Value) Value = @ptrCast(@alignCast(f.closure.function));
    const result = fp(f.closure.env, a, b);
    drop(f);
    return result;
}

pub fn call3(f: Value, a: Value, b: Value, c: Value) Value {
    const fp: *const fn ([]const Value, Value, Value, Value) Value = @ptrCast(@alignCast(f.closure.function));
    const result = fp(f.closure.env, a, b, c);
    drop(f);
    return result;
}

pub fn call4(f: Value, a: Value, b: Value, c: Value, d: Value) Value {
    const fp: *const fn ([]const Value, Value, Value, Value, Value) Value = @ptrCast(@alignCast(f.closure.function));
    const result = fp(f.closure.env, a, b, c, d);
    drop(f);
    return result;
}

pub fn call5(f: Value, a: Value, b: Value, c: Value, d: Value, e: Value) Value {
    const fp: *const fn ([]const Value, Value, Value, Value, Value, Value) Value = @ptrCast(@alignCast(f.closure.function));
    const result = fp(f.closure.env, a, b, c, d, e);
    drop(f);
    return result;
}

pub fn call6(f: Value, a: Value, b: Value, c: Value, d: Value, e: Value, g: Value) Value {
    const fp: *const fn ([]const Value, Value, Value, Value, Value, Value, Value) Value = @ptrCast(@alignCast(f.closure.function));
    const result = fp(f.closure.env, a, b, c, d, e, g);
    drop(f);
    return result;
}

// ------------------------------------------------------------- reuse (FBIP)

/// Consume a cons cell matched by `[head, ..tail]` whose clause will build
/// a same-shaped cell. When the subject is unshared (rc == 1) the cell is
/// stolen for reuse: its field references are released (the clause's
/// bindings hold their own) and the cell returned with its count intact.
/// When shared, this is an ordinary drop and construction will allocate.
pub fn dropReuseCons(subject: Value) ?*Cons {
    const cell = @constCast(subject.list.?);
    if (cell.rc == 1) {
        drop(cell.head);
        dropList(cell.tail);
        return cell;
    }
    cell.rc -= 1;
    return null;
}

/// Consumes head and tail; writes into the reuse cell when one is
/// available, allocating otherwise.
pub fn consReuse(token: ?*Cons, head: Value, tail: Value) Value {
    if (token) |cell| {
        cell.head = head;
        cell.tail = tail.list;
        return Value{ .list = cell };
    }
    return cons(head, tail);
}

// ------------------------------------------------------------ threads

/// Deep-copy a value for transfer across a thread boundary. Reference
/// counts are non-atomic, so values are never shared between threads;
/// the copy has fresh counts owned entirely by the receiving thread.
/// Closure code pointers are shared (code is immutable); their captured
/// environments are copied.
pub fn deepCopy(value: Value) Value {
    return switch (value) {
        .int, .float, .bool, .nil => value,
        .string => |s| copyString(s),
        .list => {
            // Copy the spine iteratively, then reverse into a fresh list.
            var reversed: Value = emptyList();
            var cell = value.list;
            while (cell) |c| : (cell = c.tail) {
                reversed = cons(deepCopy(c.head), reversed);
            }
            var result: Value = emptyList();
            cell = reversed.list;
            while (cell) |c| : (cell = c.tail) {
                result = cons(dup(c.head), result);
            }
            drop(reversed);
            return result;
        },
        .tuple => |t| {
            if (t.len == 0) return value;
            const copied = allocValueSlice(t.len);
            for (t, 0..) |element, index| copied[index] = deepCopy(element);
            return Value{ .tuple = copied };
        },
        .record => |r| {
            const record = rc_allocator().create(Record) catch @panic("out of memory");
            var fields: []const Value = &.{};
            if (r.fields.len != 0) {
                const copied = allocValueSlice(r.fields.len);
                for (r.fields, 0..) |field, index| copied[index] = deepCopy(field);
                fields = copied;
            }
            record.* = Record{ .rc = 1, .name = r.name, .fields = fields, .labels = r.labels };
            return Value{ .record = record };
        },
        .closure => |c| {
            if (c.env.len == 0) return value;
            const copied = allocValueSlice(c.env.len);
            for (c.env, 0..) |element, index| copied[index] = deepCopy(element);
            return Value{ .closure = Closure{ .function = c.function, .env = copied } };
        },
        .bit_array => |b| copyBitArray(b.bytes()),
    };
}

// --------------------------------------------------------- bit arrays

/// Wrap owned buffer bytes (an allocString payload) without copying.
fn bitArrayFromOwnedBuffer(buffer: []const u8, offset: usize, length: usize) Value {
    const array = rc_allocator().create(BitArray) catch @panic("out of memory");
    array.* = BitArray{ .rc = 1, .buffer = buffer, .offset = offset, .length = length };
    return Value{ .bit_array = array };
}

pub fn emptyBitArray() Value {
    return bitArrayFromOwnedBuffer(&.{}, 0, 0);
}

/// Copy bytes into a fresh bit array (used by FFI).
pub fn copyBitArray(bytes: []const u8) Value {
    if (bytes.len == 0) return emptyBitArray();
    const owned = allocString(bytes.len);
    @memcpy(owned, bytes);
    return bitArrayFromOwnedBuffer(owned, 0, bytes.len);
}

/// Construction builder. Backed by scratch memory; finish() copies into a
/// reference-counted buffer.
pub const BitArrayBuilder = struct {
    bytes: std.ArrayList(u8) = .empty,

    fn append(self: *BitArrayBuilder, data: []const u8) void {
        self.bytes.appendSlice(allocator, data) catch @panic("out of memory");
    }
};

pub fn baBuilder() BitArrayBuilder {
    return .{};
}

/// Consumes the value. Writes `bits` (a multiple of 8) of the integer.
/// Segments wider than 64 bits sign-extend (the value representation is
/// i64; other targets would carry a bignum here).
pub fn baAddInt(builder: *BitArrayBuilder, value: Value, bits: usize, little: bool) void {
    const byte_count = bits / 8;
    var v: i64 = value.int;
    const start = builder.bytes.items.len;
    builder.bytes.resize(allocator, start + byte_count) catch @panic("out of memory");
    const out = builder.bytes.items[start..];
    var index: usize = 0;
    while (index < byte_count) : (index += 1) {
        const byte: u8 = @truncate(@as(u64, @bitCast(v)));
        if (little) {
            out[index] = byte;
        } else {
            out[byte_count - 1 - index] = byte;
        }
        // Arithmetic shift keeps the sign fill for wide segments.
        v >>= 8;
    }
}

/// Consumes the value.
pub fn baAddFloat(builder: *BitArrayBuilder, value: Value, bits: usize, little: bool) void {
    if (bits == 16) {
        const as16: f16 = @floatCast(value.float);
        var raw: u16 = @bitCast(as16);
        var buffer: [2]u8 = undefined;
        for (0..2) |index| {
            if (little) buffer[index] = @truncate(raw) else buffer[1 - index] = @truncate(raw);
            raw >>= 8;
        }
        builder.append(&buffer);
        return;
    }
    if (bits == 32) {
        const as32: f32 = @floatCast(value.float);
        var raw: u32 = @bitCast(as32);
        var buffer: [4]u8 = undefined;
        for (0..4) |index| {
            if (little) buffer[index] = @truncate(raw) else buffer[3 - index] = @truncate(raw);
            raw >>= 8;
        }
        builder.append(&buffer);
    } else {
        var raw: u64 = @bitCast(value.float);
        var buffer: [8]u8 = undefined;
        for (0..8) |index| {
            if (little) buffer[index] = @truncate(raw) else buffer[7 - index] = @truncate(raw);
            raw >>= 8;
        }
        builder.append(&buffer);
    }
}

/// Consumes the string value.
pub fn baAddUtf8(builder: *BitArrayBuilder, value: Value) void {
    builder.append(value.string);
    drop(value);
}

/// Consumes the int value (a codepoint).
pub fn baAddUtf8Codepoint(builder: *BitArrayBuilder, value: Value) void {
    var buffer: [4]u8 = undefined;
    const length = std.unicode.utf8Encode(@intCast(value.int), &buffer) catch 0;
    builder.append(buffer[0..length]);
}

/// Consumes the bit array value.
pub fn baAddBits(builder: *BitArrayBuilder, value: Value) void {
    builder.append(value.bit_array.bytes());
    drop(value);
}

pub fn baFinish(builder: *BitArrayBuilder) Value {
    defer builder.bytes.deinit(allocator);
    return copyBitArray(builder.bytes.items);
}

/// Pattern matcher. Extractors run left to right as short-circuit
/// conditions, advancing the cursor and storing raw (non reference
/// counted) values in slots, so a failed match owns nothing.
pub const BitArrayMatcher = struct {
    data: []const u8,
    cursor: usize = 0,
    ints: [16]i64 = @splat(0),
    /// (offset, length) pairs for rest-slice bindings.
    marks: [16][2]usize = @splat(.{ 0, 0 }),
};

/// Borrows the subject.
pub fn baMatcher(subject: Value) BitArrayMatcher {
    return .{ .data = subject.bit_array.bytes() };
}

/// Size of an int/float segment in bits, from a runtime value.
pub fn baBitCount(value: Value) usize {
    const bits: usize = @intCast(value.int);
    if (bits % 8 != 0) @panic("non-byte-aligned bit array segments are not supported yet");
    return bits;
}

/// Bits-to-bytes for pattern sizes; negative or misaligned values yield
/// -1, which the byte reader rejects (match failure).
pub fn baBitsToBytes(bits: i64) i64 {
    if (bits < 0 or @rem(bits, 8) != 0) return -1;
    return @divTrunc(bits, 8);
}

pub fn baReadInt(
    matcher: *BitArrayMatcher,
    bits_raw: i64,
    signed: bool,
    little: bool,
    slot: usize,
) bool {
    // A negative or misaligned runtime size fails the match, as on the
    // other targets.
    if (bits_raw < 0 or @rem(bits_raw, 8) != 0) return false;
    const bits: usize = @intCast(bits_raw);
    const byte_count = bits / 8;
    if (matcher.cursor + byte_count > matcher.data.len) return false;
    var raw: u64 = 0;
    const view = matcher.data[matcher.cursor .. matcher.cursor + byte_count];
    for (0..byte_count) |index| {
        const byte = if (little) view[byte_count - 1 - index] else view[index];
        raw = (raw << 8) | byte;
    }
    if (signed and bits < 64 and byte_count > 0) {
        const shift: u6 = @intCast(64 - bits);
        raw = @bitCast(@as(i64, @bitCast(raw << shift)) >> shift);
    }
    matcher.ints[slot] = @bitCast(raw);
    matcher.cursor += byte_count;
    return true;
}

pub fn baReadFloat(matcher: *BitArrayMatcher, bits_raw: i64, little: bool, slot: usize) bool {
    if (bits_raw < 0 or @rem(bits_raw, 8) != 0) return false;
    const bits: usize = @intCast(bits_raw);
    const byte_count = bits / 8;
    if (matcher.cursor + byte_count > matcher.data.len) return false;
    var raw: u64 = 0;
    const view = matcher.data[matcher.cursor .. matcher.cursor + byte_count];
    for (0..byte_count) |index| {
        const byte = if (little) view[byte_count - 1 - index] else view[index];
        raw = (raw << 8) | byte;
    }
    const value: f64 = switch (bits) {
        16 => @floatCast(@as(f16, @bitCast(@as(u16, @truncate(raw))))),
        32 => @floatCast(@as(f32, @bitCast(@as(u32, @truncate(raw))))),
        else => @bitCast(raw),
    };
    matcher.ints[slot] = @bitCast(value);
    matcher.cursor += byte_count;
    return true;
}

pub fn baReadUtf8Codepoint(matcher: *BitArrayMatcher, slot: usize) bool {
    if (matcher.cursor >= matcher.data.len) return false;
    const length = std.unicode.utf8ByteSequenceLength(matcher.data[matcher.cursor]) catch
        return false;
    if (matcher.cursor + length > matcher.data.len) return false;
    const view = matcher.data[matcher.cursor .. matcher.cursor + length];
    const codepoint = std.unicode.utf8Decode(view) catch return false;
    matcher.ints[slot] = codepoint;
    matcher.cursor += length;
    return true;
}

/// Fixed-size bytes segment; records a mark for the binding.
pub fn baReadBytes(matcher: *BitArrayMatcher, byte_count_raw: i64, slot: usize) bool {
    if (byte_count_raw < 0) return false;
    const byte_count: usize = @intCast(byte_count_raw);
    if (matcher.cursor + byte_count > matcher.data.len) return false;
    matcher.marks[slot] = .{ matcher.cursor, byte_count };
    matcher.cursor += byte_count;
    return true;
}

/// Rest-of-input segment (must be last); always matches.
pub fn baReadRest(matcher: *BitArrayMatcher, slot: usize) bool {
    matcher.marks[slot] = .{ matcher.cursor, matcher.data.len - matcher.cursor };
    matcher.cursor = matcher.data.len;
    return true;
}

/// Literal utf8 prefix (e.g. `<<"ok":utf8, ..>>` patterns).
pub fn baMatchLiteral(matcher: *BitArrayMatcher, literal: []const u8) bool {
    if (matcher.cursor + literal.len > matcher.data.len) return false;
    if (!std.mem.eql(u8, matcher.data[matcher.cursor .. matcher.cursor + literal.len], literal)) {
        return false;
    }
    matcher.cursor += literal.len;
    return true;
}

/// The whole input must have been consumed for the pattern to match.
pub fn baAtEnd(matcher: *BitArrayMatcher) bool {
    return matcher.cursor == matcher.data.len;
}

pub fn baIntSlot(matcher: *BitArrayMatcher, slot: usize) Value {
    return intValue(matcher.ints[slot]);
}

pub fn baFloatSlot(matcher: *BitArrayMatcher, slot: usize) Value {
    return floatValue(@bitCast(matcher.ints[slot]));
}

/// Owned view sharing the subject's buffer (borrows the subject).
pub fn baSliceSlot(matcher: *BitArrayMatcher, subject: Value, slot: usize) Value {
    const mark = matcher.marks[slot];
    const source = subject.bit_array;
    if (source.buffer.len != 0) stringRc(source.buffer).* += 1;
    const array = rc_allocator().create(BitArray) catch @panic("out of memory");
    array.* = BitArray{
        .rc = 1,
        .buffer = source.buffer,
        .offset = source.offset + mark[0],
        .length = mark[1],
    };
    return Value{ .bit_array = array };
}

// --------------------------------------------------------- pattern support
// Pattern helpers BORROW the subject: the subject temporary stays owned by
// the enclosing case and is dropped once, after a clause is selected.

pub fn stringStartsWith(subject: Value, prefix: []const u8) bool {
    return subject.string.len >= prefix.len and
        std.mem.eql(u8, subject.string[0..prefix.len], prefix);
}

/// Borrows the subject, returns an owned copy of the remainder.
pub fn stringDropPrefix(subject: Value, prefix_length: usize) Value {
    return copyString(subject.string[prefix_length..]);
}

/// Borrows: string pattern test against a static literal.
pub fn stringLiteralEquals(value: Value, literal: []const u8) bool {
    return std.mem.eql(u8, value.string, literal);
}

pub fn recordHasName(value: Value, name: []const u8) bool {
    return std.mem.eql(u8, value.record.name, name);
}

// ---------------------------------------------------------------- equality

/// Borrows both values (used by FFI and internally).
pub fn isEqual(a: Value, b: Value) bool {
    if (std.meta.activeTag(a) != std.meta.activeTag(b)) return false;
    return switch (a) {
        .int => a.int == b.int,
        .float => a.float == b.float,
        .bool => a.bool == b.bool,
        .string => std.mem.eql(u8, a.string, b.string),
        .nil => true,
        .list => {
            var left = a.list;
            var right = b.list;
            while (left != null and right != null) {
                if (!isEqual(left.?.head, right.?.head)) return false;
                left = left.?.tail;
                right = right.?.tail;
            }
            return left == null and right == null;
        },
        .tuple => {
            if (a.tuple.len != b.tuple.len) return false;
            for (a.tuple, b.tuple) |x, y| {
                if (!isEqual(x, y)) return false;
            }
            return true;
        },
        .record => {
            if (!std.mem.eql(u8, a.record.name, b.record.name)) return false;
            if (a.record.fields.len != b.record.fields.len) return false;
            for (a.record.fields, b.record.fields) |x, y| {
                if (!isEqual(x, y)) return false;
            }
            return true;
        },
        // Function equality is reference equality, as on other targets.
        .closure => a.closure.function == b.closure.function and
            a.closure.env.ptr == b.closure.env.ptr,
        .bit_array => std.mem.eql(u8, a.bit_array.bytes(), b.bit_array.bytes()),
    };
}

/// Consumes both operands.
pub fn eq(a: Value, b: Value) Value {
    const result = boolValue(isEqual(a, b));
    drop(a);
    drop(b);
    return result;
}

/// Consumes both operands.
pub fn notEq(a: Value, b: Value) Value {
    const result = boolValue(!isEqual(a, b));
    drop(a);
    drop(b);
    return result;
}

/// Consumes both operands.
pub fn concatenate(a: Value, b: Value) Value {
    if (a.string.len + b.string.len == 0) {
        drop(a);
        drop(b);
        return Value{ .string = &.{} };
    }
    const out = allocString(a.string.len + b.string.len);
    @memcpy(out[0..a.string.len], a.string);
    @memcpy(out[a.string.len..], b.string);
    drop(a);
    drop(b);
    return Value{ .string = out };
}

// -------------------------------------------------------------- inspection

fn inspect(writer: anytype, value: Value) void {
    switch (value) {
        .int => |i| writer.print("{d}", .{i}) catch {},
        .float => |f| {
            // Gleam floats always show a decimal point: 1.0, not 1.
            if (f == @trunc(f) and !std.math.isInf(f) and !std.math.isNan(f)) {
                writer.print("{d}.0", .{f}) catch {};
            } else {
                writer.print("{d}", .{f}) catch {};
            }
        },
        .bool => |b| writer.print("{s}", .{if (b) "True" else "False"}) catch {},
        .string => |s| {
            writer.print("\"", .{}) catch {};
            for (s) |c| {
                switch (c) {
                    '"' => writer.print("\\\"", .{}) catch {},
                    '\\' => writer.print("\\\\", .{}) catch {},
                    '\n' => writer.print("\\n", .{}) catch {},
                    '\r' => writer.print("\\r", .{}) catch {},
                    '\t' => writer.print("\\t", .{}) catch {},
                    else => writer.print("{c}", .{c}) catch {},
                }
            }
            writer.print("\"", .{}) catch {};
        },
        .nil => writer.print("Nil", .{}) catch {},
        .list => {
            writer.print("[", .{}) catch {};
            var cell = value.list;
            var first = true;
            while (cell != null) {
                if (!first) writer.print(", ", .{}) catch {};
                first = false;
                inspect(writer, cell.?.head);
                cell = cell.?.tail;
            }
            writer.print("]", .{}) catch {};
        },
        .tuple => {
            writer.print("#(", .{}) catch {};
            for (value.tuple, 0..) |element, index| {
                if (index != 0) writer.print(", ", .{}) catch {};
                inspect(writer, element);
            }
            writer.print(")", .{}) catch {};
        },
        .record => {
            writer.print("{s}", .{value.record.name}) catch {};
            if (value.record.fields.len != 0) {
                writer.print("(", .{}) catch {};
                for (value.record.fields, 0..) |field, index| {
                    if (index != 0) writer.print(", ", .{}) catch {};
                    if (index < value.record.labels.len) {
                        if (value.record.labels[index]) |label| {
                            writer.print("{s}: ", .{label}) catch {};
                        }
                    }
                    inspect(writer, field);
                }
                writer.print(")", .{}) catch {};
            }
        },
        .closure => writer.print("//fn", .{}) catch {},
        .bit_array => |b| {
            writer.print("<<", .{}) catch {};
            for (b.bytes(), 0..) |byte, index| {
                if (index != 0) writer.print(", ", .{}) catch {};
                writer.print("{d}", .{byte}) catch {};
            }
            writer.print(">>", .{}) catch {};
        },
    }
}

/// Borrows the value; renders it in Gleam syntax as an owned string.
pub fn inspectValue(value: Value) Value {
    var aw = std.Io.Writer.Allocating.init(allocator);
    defer aw.deinit();
    inspect(&aw.writer, value);
    return copyString(aw.written());
}

/// `echo` prints "file:line" then the inspected value to stderr and
/// returns the (still owned) value, matching the JavaScript target's echo.
pub fn echo(value: Value, file: []const u8, line: u32) Value {
    var buffer: [4096]u8 = undefined;
    const stderr = std.debug.lockStderr(&buffer);
    defer std.debug.unlockStderr();
    const w = &stderr.file_writer.interface;
    w.print("\x1b[90m{s}:{d}\x1b[39m\n", .{ file, line }) catch {};
    inspect(w, value);
    w.print("\n", .{}) catch {};
    w.flush() catch {};
    return value;
}

/// Consumes the message (the process exits).
pub fn gleamPanic(message: []const u8, file: []const u8, line: u32) noreturn {
    {
        var buffer: [4096]u8 = undefined;
        const stderr = std.debug.lockStderr(&buffer);
        defer std.debug.unlockStderr();
        const w = &stderr.file_writer.interface;
        w.print("{s}:{d} panic: {s}\n", .{ file, line, message }) catch {};
        w.flush() catch {};
    }
    std.process.exit(1);
}
