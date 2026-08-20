// Gleam Zig target runtime prelude.
//
// Values use a uniform tagged-union representation, mirroring the dynamic
// representation of the JavaScript target. Memory is currently leaked;
// Perceus reference counting lands as a later compiler pass.
//
// Int is i64 with wrapping arithmetic (the JavaScript target uses f64
// numbers, Erlang has bignums; targets choose a pragmatic representation).

const std = @import("std");

pub const Value = union(enum) {
    int: i64,
    float: f64,
    bool: bool,
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
    head: Value,
    tail: ?*const Cons,
};

pub const Record = struct {
    /// Variant name, e.g. "Ok". Variant identity is (name, arity), which is
    /// unique within a type, and values of different types never meet in a
    /// well-typed pattern match.
    name: []const u8,
    fields: []const Value,
};

/// All function values share one shape: a type-erased pointer to a lifted
/// function whose first parameter is the captured environment. Call sites
/// know the arity statically and cast through callN below.
pub const Closure = struct {
    function: *const anyopaque,
    env: []const Value,
};

pub const allocator = std.heap.page_allocator;

fn alloc(comptime T: type) *T {
    return allocator.create(T) catch @panic("out of memory");
}

pub fn dupeValues(values: []const Value) []const Value {
    return allocator.dupe(Value, values) catch @panic("out of memory");
}

pub fn intValue(i: i64) Value {
    return Value{ .int = i };
}

pub fn floatValue(f: f64) Value {
    return Value{ .float = f };
}

pub fn boolValue(b: bool) Value {
    return Value{ .bool = b };
}

pub fn stringValue(s: []const u8) Value {
    return Value{ .string = s };
}

pub const NIL = Value{ .nil = {} };
pub const TRUE = Value{ .bool = true };
pub const FALSE = Value{ .bool = false };

// Int maths. Wrapping, matching no-overflow-panic semantics of the other
// targets (which never overflow).

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

// Float maths.

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

// Comparisons.

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

// Value construction.

pub fn emptyList() Value {
    return Value{ .list = null };
}

pub fn listValue(cell: ?*const Cons) Value {
    return Value{ .list = cell };
}

pub fn recordHasName(value: Value, name: []const u8) bool {
    return std.mem.eql(u8, value.record.name, name);
}

pub fn cons(head: Value, tail: Value) Value {
    const cell = alloc(Cons);
    cell.* = Cons{ .head = head, .tail = tail.list };
    return Value{ .list = cell };
}

/// Build a list from elements and an optional tail list.
pub fn listFromSlice(elements: []const Value, tail: Value) Value {
    var result = tail;
    var index = elements.len;
    while (index > 0) {
        index -= 1;
        result = cons(elements[index], result);
    }
    return result;
}

pub fn tupleValue(elements: []const Value) Value {
    return Value{ .tuple = dupeValues(elements) };
}

pub fn makeRecord(name: []const u8, fields: []const Value) Value {
    const record = alloc(Record);
    record.* = Record{ .name = name, .fields = dupeValues(fields) };
    return Value{ .record = record };
}

pub fn makeClosure(function: *const anyopaque, env: []const Value) Value {
    return Value{ .closure = Closure{ .function = function, .env = dupeValues(env) } };
}

// Closure calls, by arity.

pub fn call0(f: Value) Value {
    const fp: *const fn ([]const Value) Value = @ptrCast(@alignCast(f.closure.function));
    return fp(f.closure.env);
}

pub fn call1(f: Value, a: Value) Value {
    const fp: *const fn ([]const Value, Value) Value = @ptrCast(@alignCast(f.closure.function));
    return fp(f.closure.env, a);
}

pub fn call2(f: Value, a: Value, b: Value) Value {
    const fp: *const fn ([]const Value, Value, Value) Value = @ptrCast(@alignCast(f.closure.function));
    return fp(f.closure.env, a, b);
}

pub fn call3(f: Value, a: Value, b: Value, c: Value) Value {
    const fp: *const fn ([]const Value, Value, Value, Value) Value = @ptrCast(@alignCast(f.closure.function));
    return fp(f.closure.env, a, b, c);
}

pub fn call4(f: Value, a: Value, b: Value, c: Value, d: Value) Value {
    const fp: *const fn ([]const Value, Value, Value, Value, Value) Value = @ptrCast(@alignCast(f.closure.function));
    return fp(f.closure.env, a, b, c, d);
}

pub fn call5(f: Value, a: Value, b: Value, c: Value, d: Value, e: Value) Value {
    const fp: *const fn ([]const Value, Value, Value, Value, Value, Value) Value = @ptrCast(@alignCast(f.closure.function));
    return fp(f.closure.env, a, b, c, d, e);
}

pub fn call6(f: Value, a: Value, b: Value, c: Value, d: Value, e: Value, g: Value) Value {
    const fp: *const fn ([]const Value, Value, Value, Value, Value, Value, Value) Value = @ptrCast(@alignCast(f.closure.function));
    return fp(f.closure.env, a, b, c, d, e, g);
}

// String prefix matching, for `"prefix" <> rest` patterns.
pub fn stringStartsWith(subject: Value, prefix: []const u8) bool {
    return subject.string.len >= prefix.len and
        std.mem.eql(u8, subject.string[0..prefix.len], prefix);
}

pub fn stringDropPrefix(subject: Value, prefix_length: usize) Value {
    return stringValue(subject.string[prefix_length..]);
}

// Structural equality.
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

pub fn eq(a: Value, b: Value) Value {
    return boolValue(isEqual(a, b));
}

pub fn notEq(a: Value, b: Value) Value {
    return boolValue(!isEqual(a, b));
}

// String concatenation. Leaked, like all allocation for now.
pub fn concatenate(a: Value, b: Value) Value {
    const out = allocator.alloc(u8, a.string.len + b.string.len) catch @panic("out of memory");
    @memcpy(out[0..a.string.len], a.string);
    @memcpy(out[a.string.len..], b.string);
    return stringValue(out);
}

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
                    inspect(writer, field);
                }
                writer.print(")", .{}) catch {};
            }
        },
        .closure => writer.print("//fn", .{}) catch {},
    }
}

/// Bit arrays have no zig representation yet. Functions containing bit
/// array patterns or literals still compile; reaching one at runtime panics.
pub fn unsupportedBitArrayPattern() bool {
    @panic("BitArray is not supported on the zig target yet");
}

pub fn unsupportedBitArray() Value {
    @panic("BitArray is not supported on the zig target yet");
}

/// Render a value in Gleam syntax, for string.inspect and friends.
pub fn inspectValue(value: Value) []const u8 {
    var aw = std.Io.Writer.Allocating.init(allocator);
    inspect(&aw.writer, value);
    return aw.written();
}

/// `echo` prints "file:line" then the inspected value to stderr and
/// returns the value, matching the JavaScript target's echo.
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
