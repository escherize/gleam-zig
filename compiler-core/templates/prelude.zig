// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 The Gleam contributors

// Gleam Zig target runtime prelude.
//
// Values use a uniform tagged-union representation, mirroring the dynamic
// representation of the JavaScript target. Memory is reference counted
// (Perceus phase 1: naive counting, no reuse or borrowing yet).
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

/// Reference-counted allocations come from a leak-checking allocator;
/// `leakCheckExit` reports anything still live after `main`.
pub var debug_allocator: std.heap.DebugAllocator(.{}) = .init;

fn rc_allocator() std.mem.Allocator {
    return debug_allocator.allocator();
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

fn allocValueSlice(count: usize) []Value {
    const words = rc_allocator().alloc(Value, count + 1) catch @panic("out of memory");
    words[0] = Value{ .int = 1 };
    return words[1..];
}

fn valueSliceRc(payload: []const Value) *i64 {
    const base: [*]Value = @constCast(payload.ptr) - 1;
    return &base[0].int;
}

fn freeValueSlice(payload: []const Value) void {
    const base: [*]Value = @constCast(payload.ptr) - 1;
    rc_allocator().free(base[0 .. payload.len + 1]);
}

fn stringWordCount(byte_length: usize) usize {
    return 1 + (byte_length + 7) / 8;
}

fn allocString(byte_length: usize) []u8 {
    const words = rc_allocator().alloc(u64, stringWordCount(byte_length)) catch
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
    rc_allocator().free(base[0..stringWordCount(payload.len)]);
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
                rc_allocator().destroy(mutable);
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
        rc_allocator().destroy(mutable);
        cell = next;
    }
}

/// Report leaked reference-counted allocations after main returns.
pub fn leakCheckExit() void {
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
    const cell = rc_allocator().create(Cons) catch @panic("out of memory");
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
    const record = rc_allocator().create(Record) catch @panic("out of memory");
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

/// Bit arrays have no zig representation yet. Functions containing bit
/// array patterns or literals still compile; reaching one at runtime panics.
pub fn unsupportedBitArrayPattern() bool {
    @panic("BitArray is not supported on the zig target yet");
}

pub fn unsupportedBitArray() Value {
    @panic("BitArray is not supported on the zig target yet");
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
