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
};

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

// Structural equality.
pub fn isEqual(a: Value, b: Value) bool {
    if (std.meta.activeTag(a) != std.meta.activeTag(b)) return false;
    return switch (a) {
        .int => a.int == b.int,
        .float => a.float == b.float,
        .bool => a.bool == b.bool,
        .string => std.mem.eql(u8, a.string, b.string),
        .nil => true,
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
    const alloc = std.heap.page_allocator;
    const out = alloc.alloc(u8, a.string.len + b.string.len) catch @panic("out of memory");
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
    }
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
