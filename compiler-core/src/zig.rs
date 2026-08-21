// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 The Gleam contributors

// Zig target code generation.
//
// Values use the uniform `Value` union defined in templates/prelude.zig,
// except scalar (Int/Float/Bool) expressions and locals, which are emitted
// as raw i64/f64/bool and boxed only at polymorphic boundaries. Module
// functions whose signature is entirely concrete scalars are emitted with
// a raw native ABI plus a boxed public wrapper. Pattern matching
// compiles to sequential per-clause checks rather than decision trees; the
// type checker has already proven exhaustiveness so a fallthrough is
// `unreachable`. Unsupported constructs (bit arrays, external fns) panic
// with "zig codegen" in the message.

use std::collections::{BTreeSet, HashMap, HashSet};

use ecow::EcoString;
use itertools::Itertools;
use src_span::{LineNumbers, SrcSpan};

use crate::ast::{
    AssignName, AssignmentKind, BinOp, CallArg, Constant, Pattern, Statement,
    TypedAssignment, TypedClause, TypedClauseGuard, TypedExpr, TypedFunction, TypedModule,
    TypedPattern, TypedStatement,
};
use crate::type_::{ModuleValueConstructor, PRELUDE_MODULE_NAME, ValueConstructorVariant};

pub const PRELUDE: &str = include_str!("../templates/prelude.zig");

const INDENT: &str = "    ";

/// A Gleam type with an unboxed zig representation.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ScalarKind {
    Int,
    Float,
    Bool,
}

impl ScalarKind {
    /// The `Value` union field holding this scalar.
    fn field(self) -> &'static str {
        match self {
            ScalarKind::Int => "int",
            ScalarKind::Float => "float",
            ScalarKind::Bool => "bool",
        }
    }

    fn zig_type(self) -> &'static str {
        match self {
            ScalarKind::Int => "i64",
            ScalarKind::Float => "f64",
            ScalarKind::Bool => "bool",
        }
    }

    /// The prelude helper wrapping a raw scalar into a `Value`.
    fn box_helper(self) -> &'static str {
        match self {
            ScalarKind::Int => "P.intValue",
            ScalarKind::Float => "P.floatValue",
            ScalarKind::Bool => "P.boolValue",
        }
    }
}

fn scalar_kind(type_: &crate::type_::Type) -> Option<ScalarKind> {
    if type_.is_int() {
        Some(ScalarKind::Int)
    } else if type_.is_float() {
        Some(ScalarKind::Float)
    } else if type_.is_bool() {
        Some(ScalarKind::Bool)
    } else {
        None
    }
}

pub fn module(
    module: &TypedModule,
    line_numbers: &LineNumbers,
    src_path: &str,
    prelude_import_path: &str,
) -> String {
    let mut imported_packages: HashMap<EcoString, EcoString> = HashMap::new();
    for import in &module.definitions.imports {
        let _ = imported_packages.insert(import.module.clone(), import.package.clone());
    }

    // Functions whose signature is entirely concrete scalars get a raw
    // native ABI; computed up front so call sites anywhere in the module
    // (including bodies generated before the callee) can use it.
    let mut native_signatures = HashMap::new();
    for function in &module.definitions.functions {
        let Some((_, name)) = &function.name else {
            continue;
        };
        if function.external_zig.is_some()
            || function.body.is_empty()
            || !function.implementations.supports(crate::build::Target::Zig)
        {
            continue;
        }
        let Some(return_kind) = scalar_kind(&function.return_type) else {
            continue;
        };
        let Some(parameter_kinds) = function
            .arguments
            .iter()
            .map(|argument| scalar_kind(&argument.type_))
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        let _ = native_signatures.insert(name.clone(), (parameter_kinds, return_kind));
    }

    // Functions whose parameters are only ever field-read get a borrowed
    // ABI: same-module callers pass references without dup, the callee
    // never drops. Scalar parameters carry no references and are always
    // borrowable; boxed parameters qualify when every occurrence is the
    // container of a field access. The public name keeps the owned
    // convention through a wrapper.
    let mut borrowed_signatures: HashMap<EcoString, Vec<bool>> = HashMap::new();
    for function in &module.definitions.functions {
        let Some((_, name)) = &function.name else {
            continue;
        };
        if function.external_zig.is_some()
            || function.body.is_empty()
            || !function.implementations.supports(crate::build::Target::Zig)
            || native_signatures.contains_key(name)
            || function.arguments.is_empty()
            // Tail-call loops reassign their parameters with owned
            // values; mixing conventions there is not worth it.
            || body_has_tail_self_call(&function.body, name)
        {
            continue;
        }
        let flags: Vec<bool> = function
            .arguments
            .iter()
            .map(|argument| match argument.names.get_variable_name() {
                // A discarded parameter is never used; the caller simply
                // keeps its reference.
                None => true,
                Some(parameter_name) => {
                    scalar_kind(&argument.type_).is_some()
                        || param_uses_are_borrow_only(parameter_name, &function.body)
                }
            })
            .collect();
        // A second ABI only pays when a boxed parameter is borrowed.
        let worthwhile = function
            .arguments
            .iter()
            .zip(&flags)
            .any(|(argument, borrowed)| {
                *borrowed && scalar_kind(&argument.type_).is_none()
            });
        if worthwhile {
            let _ = borrowed_signatures.insert(name.clone(), flags);
        }
    }

    let mut shared = ModuleContext {
        module_name: module.name.clone(),
        line_numbers,
        src_path,
        imported_packages,
        modules_used: BTreeSet::new(),
        lifted: Vec::new(),
        lambda_counter: 0,
        wrapper_cache: HashMap::new(),
        ffi_imports: std::collections::BTreeMap::new(),
        native_signatures,
        borrowed_signatures,
    };

    let mut functions = String::new();
    // Constants become zero-argument functions: their values may allocate
    // (records, lists), which zig cannot do in a comptime const initializer.
    for constant in &module.definitions.constants {
        let mut generator = FunctionGenerator::new(&mut shared);
        let value = generator.constant(&constant.value);
        let visibility = if constant.publicity.is_private() {
            ""
        } else {
            "pub "
        };
        functions.push_str(&format!(
            "{visibility}fn {}() Value {{\n{INDENT}return {value};\n}}\n\n",
            constant_identifier(&constant.name),
        ));
    }
    for function in &module.definitions.functions {
        let mut generator = FunctionGenerator::new(&mut shared);
        functions.push_str(&generator.function(function));
        functions.push('\n');
    }

    let mut out = String::new();
    out.push_str("// Generated by the Gleam compiler. Do not edit.\n");
    out.push_str(&format!(
        "const P = @import(\"{prelude_import_path}\");\nconst Value = P.Value;\n"
    ));
    // Import paths climb to the target directory root (one level per path
    // segment of this module, plus one for its package directory) and then
    // descend into the imported module's package.
    let ups = "../".repeat(module.name.split('/').count());
    for module_name in &shared.modules_used {
        let package = shared
            .imported_packages
            .get(module_name)
            .cloned()
            .unwrap_or_else(|| panic!("zig codegen: no import for module {module_name}"));
        out.push_str(&format!(
            "const {} = @import(\"{ups}{package}/{module_name}.zig\");\n",
            module_ref(module_name),
        ));
    }
    for (path, identifier) in &shared.ffi_imports {
        out.push_str(&format!("const {identifier} = @import(\"{path}\");\n"));
    }
    out.push('\n');
    out.push_str(&functions);
    for lifted in &shared.lifted {
        out.push_str(lifted);
        out.push('\n');
    }
    out
}

fn module_ref(module_name: &str) -> String {
    zig_identifier(&format!("M${module_name}"))
}

fn constant_identifier(name: &str) -> String {
    zig_identifier(&format!("constant${name}"))
}

struct ModuleContext<'a> {
    module_name: EcoString,
    line_numbers: &'a LineNumbers,
    src_path: &'a str,
    imported_packages: HashMap<EcoString, EcoString>,
    modules_used: BTreeSet<EcoString>,
    /// Lifted anonymous functions and fn-as-value wrappers, emitted after
    /// the module's named functions.
    lifted: Vec<String>,
    lambda_counter: usize,
    /// (module, name, arity) -> lifted wrapper identifier for functions and
    /// constructors used as values.
    wrapper_cache: HashMap<(EcoString, EcoString, usize), String>,
    /// FFI import path -> zig import const identifier, for @external(zig).
    ffi_imports: std::collections::BTreeMap<EcoString, String>,
    /// Module functions emitted with the raw scalar ABI (`native$name`):
    /// name -> (parameter kinds, return kind). Same-module calls use the
    /// native fn directly; everything else goes through the boxed wrapper.
    native_signatures: HashMap<EcoString, (Vec<ScalarKind>, ScalarKind)>,
    /// Module functions emitted with the borrowed ABI (`borrowed$name`):
    /// name -> per-parameter borrowed flag. Same-module calls pass
    /// borrowed arguments without taking a reference; the wrapper under
    /// the original name keeps the owned convention.
    borrowed_signatures: HashMap<EcoString, Vec<bool>>,
}

impl ModuleContext<'_> {
    fn ffi_import(&mut self, path: &EcoString) -> String {
        if let Some(identifier) = self.ffi_imports.get(path) {
            return identifier.clone();
        }
        let identifier = zig_identifier(&format!("F${}", self.ffi_imports.len()));
        let _ = self.ffi_imports.insert(path.clone(), identifier.clone());
        identifier
    }
}

/// The allocation shape a reuse token was reclaimed from; a construction
/// consumes it only when the shape matches exactly.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReuseKind {
    /// zig type ?*P.Cons, feeds `[x, ..rest]` constructions.
    Cons,
    /// zig type ?*P.Record with this field count.
    Record(usize),
    /// zig type ?[]P.Value with this element count.
    Tuple(usize),
}

#[derive(Clone, Copy, PartialEq)]
enum Tail {
    /// The expression's value is returned from the function; self tail
    /// calls rewrite to parameter reassignment.
    Return,
    /// The expression's value is not in tail position.
    No,
}

struct FunctionGenerator<'a, 'm> {
    module: &'a mut ModuleContext<'m>,
    /// Source name -> rendered zig expression currently in scope.
    scope: im::HashMap<EcoString, String>,
    /// All rendered binding names used in this function, for unique renaming.
    used_names: HashSet<EcoString>,
    label_counter: usize,
    /// Set when generating a function eligible for self-tail-call loops:
    /// (gleam name, rendered parameter identifiers).
    tail_target: Option<(EcoString, Vec<String>)>,
    /// Set when a self tail call was rewritten to a loop continue.
    used_tail_loop: bool,
    /// The binding holding the value flowing through the enclosing pipeline,
    /// for bare `|> echo` steps.
    pipe_value: Option<String>,
    /// Bindings approved for the last-use move optimisation: their single
    /// use transfers the reference (no dup) and no scope-exit drop is
    /// emitted.
    moved: HashSet<String>,
    /// A reuse token (identifier + shape) available to the next matching
    /// construction rendered in the current clause body, together with
    /// the barrier level it was armed at. The token may only be consumed
    /// at the same barrier level: conditional code (nested cases, lambda
    /// bodies, short-circuit right operands, panic messages) increments
    /// the level, so a construction that might not execute can never
    /// take the token.
    reuse_token: Option<(String, ReuseKind, usize)>,
    reuse_barrier: usize,
    /// Rendered binding names holding raw (unboxed) scalars, with their
    /// kind. Raw bindings carry no references: no dup, no drop, no move
    /// bookkeeping; boxed on demand at polymorphic uses.
    raw_bindings: HashMap<String, ScalarKind>,
    /// Set while generating a native-ABI function body: `return` exits
    /// unwrap their boxed value to this raw kind.
    native_return: Option<ScalarKind>,
}

impl<'a, 'm> FunctionGenerator<'a, 'm> {
    fn new(module: &'a mut ModuleContext<'m>) -> Self {
        Self {
            module,
            scope: im::HashMap::new(),
            used_names: HashSet::new(),
            label_counter: 0,
            tail_target: None,
            used_tail_loop: false,
            pipe_value: None,
            moved: HashSet::new(),
            reuse_token: None,
            reuse_barrier: 0,
            raw_bindings: HashMap::new(),
            native_return: None,
        }
    }

    fn function(&mut self, function: &TypedFunction) -> String {
        let name = function
            .name
            .as_ref()
            .map(|(_, name)| name.clone())
            .expect("zig codegen: anonymous top level function");

        // An external zig implementation wins over a Gleam body, mirroring
        // the other targets: emit a forwarding function to the FFI module.
        if let Some((ffi_path, ffi_name, _)) = &function.external_zig {
            let import = self.module.ffi_import(ffi_path);
            let visibility = if function.publicity.is_private() {
                ""
            } else {
                "pub "
            };
            let parameters = (0..function.arguments.len())
                .map(|index| format!("{}: Value", zig_identifier(&format!("a${index}"))))
                .join(", ");
            let forwarded = (0..function.arguments.len())
                .map(|index| zig_identifier(&format!("a${index}")))
                .join(", ");
            // FFI receives borrowed values and returns an owned one; the
            // forwarding fn owns its parameters, so it drops them after.
            let drops = (0..function.arguments.len())
                .map(|index| {
                    format!("{INDENT}P.drop({});\n", zig_identifier(&format!("a${index}")))
                })
                .join("");
            return format!(
                "{visibility}fn {}({parameters}) Value {{\n{INDENT}const result = {import}.{}({forwarded});\n{drops}{INDENT}return result;\n}}\n",
                zig_identifier(&name),
                zig_identifier(ffi_name),
            );
        }

        // The function cannot run on this target (it exists for other
        // targets' externals). The type checker stops any use of it from
        // zig code, so emit nothing.
        if function.body.is_empty()
            || !function.implementations.supports(crate::build::Target::Zig)
        {
            return String::new();
        }

        let parameter_names: Vec<EcoString> = function
            .arguments
            .iter()
            .map(|argument| argument.names.get_variable_name().cloned().unwrap_or("_".into()))
            .collect();

        let uses_tail_recursion = body_has_tail_self_call(&function.body, &name);

        let visibility = if function.publicity.is_private() {
            ""
        } else {
            "pub "
        };

        if let Some((parameter_kinds, return_kind)) =
            self.module.native_signatures.get(&name).cloned()
        {
            return self.native_function(
                &name,
                function,
                &parameter_names,
                &parameter_kinds,
                return_kind,
                uses_tail_recursion,
                visibility,
            );
        }

        if let Some(flags) = self.module.borrowed_signatures.get(&name).cloned() {
            return self.borrowed_function(
                &name,
                function,
                &parameter_names,
                &flags,
                visibility,
            );
        }

        if uses_tail_recursion {
            // Parameters become mutable locals so self tail calls can
            // reassign them and continue the loop. The locals own the
            // incoming references and are dropped at every exit.
            let mut parameter_list = Vec::new();
            let mut locals = Vec::new();
            let mut local_idents = Vec::new();
            let mut dropped_params = Vec::new();
            for parameter_name in &parameter_names {
                let rendered = self.bind(parameter_name);
                let incoming = zig_identifier(&format!("p${parameter_name}"));
                parameter_list.push(format!("{incoming}: Value"));
                locals.push(format!("{INDENT}var {rendered} = {incoming};\n"));
                local_idents.push(rendered.clone());
                // A parameter whose only use is straight-line (commonly the
                // case subject) transfers its reference there each
                // iteration; reassignment at `continue` is not a use, and
                // the moved value must not appear in any drop list.
                if summarise_uses(parameter_name, &function.body).single_straight_line_use() {
                    let _ = self.moved.insert(rendered);
                } else {
                    dropped_params.push(rendered);
                }
            }
            self.tail_target = Some((name.clone(), local_idents.clone()));

            let inner_indent = format!("{INDENT}{INDENT}");
            let body =
                self.statements(&function.body, Tail::Return, &inner_indent, &dropped_params);

            format!(
                "{visibility}fn {}({}) Value {{\n{}{INDENT}while (true) {{\n{body}{INDENT}}}\n}}\n",
                zig_identifier(&name),
                parameter_list.join(", "),
                locals.join(""),
            )
        } else {
            let mut parameter_list = Vec::new();
            let mut dropped_params = Vec::new();
            for parameter_name in &parameter_names {
                let rendered = self.bind(parameter_name);
                parameter_list.push(format!("{rendered}: Value"));
                if summarise_uses(parameter_name, &function.body).single_straight_line_use() {
                    let _ = self.moved.insert(rendered);
                } else {
                    dropped_params.push(rendered);
                }
            }

            let body =
                self.statements(&function.body, Tail::Return, INDENT, &dropped_params);

            format!(
                "{visibility}fn {}({}) Value {{\n{body}}}\n",
                zig_identifier(&name),
                parameter_list.join(", "),
            )
        }
    }

    /// A function whose signature is entirely concrete scalars: the body
    /// is emitted as `native$name` taking and returning raw i64/f64/bool,
    /// plus a boxed wrapper under the original name for cross-module
    /// callers, function references and the entrypoint.
    fn native_function(
        &mut self,
        name: &EcoString,
        function: &TypedFunction,
        parameter_names: &[EcoString],
        parameter_kinds: &[ScalarKind],
        return_kind: ScalarKind,
        uses_tail_recursion: bool,
        visibility: &str,
    ) -> String {
        let native_name = zig_identifier(&format!("native${name}"));
        self.native_return = Some(return_kind);

        let mut parameter_list = Vec::new();
        // Raw parameters that the body never reads still need a use, or
        // zig rejects the declaration.
        let mut discards = String::new();
        let native = if uses_tail_recursion {
            let mut locals = Vec::new();
            let mut local_idents = Vec::new();
            for (parameter_name, kind) in parameter_names.iter().zip(parameter_kinds) {
                let rendered = self.bind(parameter_name);
                let incoming = zig_identifier(&format!("p${parameter_name}"));
                parameter_list.push(format!("{incoming}: {}", kind.zig_type()));
                locals.push(format!("{INDENT}var {rendered} = {incoming};\n"));
                if summarise_uses(parameter_name, &function.body).count == 0 {
                    discards.push_str(&format!("{INDENT}_ = {rendered};\n"));
                }
                let _ = self.raw_bindings.insert(rendered.clone(), *kind);
                local_idents.push(rendered);
            }
            self.tail_target = Some((name.clone(), local_idents));

            let inner_indent = format!("{INDENT}{INDENT}");
            let body = self.statements(&function.body, Tail::Return, &inner_indent, &[]);
            format!(
                "fn {native_name}({}) {} {{\n{}{discards}{INDENT}while (true) {{\n{body}{INDENT}}}\n}}\n",
                parameter_list.join(", "),
                return_kind.zig_type(),
                locals.join(""),
            )
        } else {
            for (parameter_name, kind) in parameter_names.iter().zip(parameter_kinds) {
                let rendered = self.bind(parameter_name);
                parameter_list.push(format!("{rendered}: {}", kind.zig_type()));
                if summarise_uses(parameter_name, &function.body).count == 0 {
                    discards.push_str(&format!("{INDENT}_ = {rendered};\n"));
                }
                let _ = self.raw_bindings.insert(rendered, *kind);
            }
            let body = self.statements(&function.body, Tail::Return, INDENT, &[]);
            format!(
                "fn {native_name}({}) {} {{\n{discards}{body}}}\n",
                parameter_list.join(", "),
                return_kind.zig_type(),
            )
        };

        let wrapper_parameters = (0..parameter_kinds.len())
            .map(|index| format!("{}: Value", zig_identifier(&format!("a${index}"))))
            .join(", ");
        let forwarded = parameter_kinds
            .iter()
            .enumerate()
            .map(|(index, kind)| {
                format!(
                    "({}).{}",
                    zig_identifier(&format!("a${index}")),
                    kind.field()
                )
            })
            .join(", ");
        let wrapper = format!(
            "{visibility}fn {}({wrapper_parameters}) Value {{\n{INDENT}return {}({native_name}({forwarded}));\n}}\n",
            zig_identifier(name),
            return_kind.box_helper(),
        );

        format!("{native}\n{wrapper}")
    }

    /// A function with borrow-only parameters: the body is emitted as
    /// `borrowed$name` where borrowed parameters carry no reference (the
    /// caller keeps ownership; no dup at use, no drop at exit), plus a
    /// wrapper under the original name keeping the owned convention for
    /// cross-module callers, function references and the entrypoint.
    fn borrowed_function(
        &mut self,
        name: &EcoString,
        function: &TypedFunction,
        parameter_names: &[EcoString],
        flags: &[bool],
        visibility: &str,
    ) -> String {
        let borrowed_name = zig_identifier(&format!("borrowed${name}"));
        let mut parameter_list = Vec::new();
        let mut dropped_params = Vec::new();
        let mut discards = String::new();
        for (parameter_name, borrowed) in parameter_names.iter().zip(flags) {
            let rendered = self.bind(parameter_name);
            parameter_list.push(format!("{rendered}: Value"));
            if *borrowed {
                // Borrowed: never dropped here, never move-approved. Its
                // uses are field reads, which borrow in place.
                if summarise_uses(parameter_name, &function.body).count == 0 {
                    discards.push_str(&format!("{INDENT}_ = {rendered};\n"));
                }
            } else if summarise_uses(parameter_name, &function.body)
                .single_straight_line_use()
            {
                let _ = self.moved.insert(rendered);
            } else {
                dropped_params.push(rendered);
            }
        }
        let body = self.statements(&function.body, Tail::Return, INDENT, &dropped_params);
        let borrowed = format!(
            "fn {borrowed_name}({}) Value {{\n{discards}{body}}}\n",
            parameter_list.join(", "),
        );

        let wrapper_parameters = (0..flags.len())
            .map(|index| format!("{}: Value", zig_identifier(&format!("a${index}"))))
            .join(", ");
        let forwarded = (0..flags.len())
            .map(|index| zig_identifier(&format!("a${index}")))
            .join(", ");
        // The wrapper owns its arguments: borrowed ones are released
        // after the call, owned ones transferred into the callee.
        let wrapper_drops = flags
            .iter()
            .enumerate()
            .filter(|(_, borrowed)| **borrowed)
            .map(|(index, _)| {
                format!("{INDENT}P.drop({});\n", zig_identifier(&format!("a${index}")))
            })
            .join("");
        let wrapper = format!(
            "{visibility}fn {}({wrapper_parameters}) Value {{\n{INDENT}const result = {borrowed_name}({forwarded});\n{wrapper_drops}{INDENT}return result;\n}}\n",
            zig_identifier(name),
        );

        format!("{borrowed}\n{wrapper}")
    }

    /// Render a statement sequence. The final statement's value is emitted
    /// according to `tail`: returned (function body, `Tail::Return`) or
    /// left as a `BREAK_PLACEHOLDER` for the enclosing labelled block.
    ///
    /// `pending_drops` are owned bindings of enclosing scopes (function
    /// parameters, outer lets, case subjects) that must be released on any
    /// `Tail::Return` exit taken from inside this sequence. This sequence's
    /// own bindings are dropped here on every exit path.
    fn statements(
        &mut self,
        statements: &[TypedStatement],
        tail: Tail,
        indent: &str,
        pending_drops: &[String],
    ) -> String {
        let saved_scope = self.scope.clone();
        let mut out = String::new();
        let mut own: Vec<String> = Vec::new();

        let last_index = statements.len() - 1;
        for (index, statement) in statements.iter().enumerate() {
            let is_last = index == last_index;
            match statement {
                Statement::Expression(expression) => {
                    if is_last {
                        let drops: Vec<String> =
                            own.iter().chain(pending_drops).cloned().collect();
                        out.push_str(&self.final_statement(expression, tail, indent, &drops));
                    } else {
                        // Unused result: release it.
                        let value = self.expression(expression, indent);
                        out.push_str(&format!("{indent}P.drop({value});\n"));
                    }
                }
                Statement::Use(use_) => {
                    if is_last {
                        let drops: Vec<String> =
                            own.iter().chain(pending_drops).cloned().collect();
                        out.push_str(&self.final_statement(&use_.call, tail, indent, &drops));
                    } else {
                        let value = self.expression(&use_.call, indent);
                        out.push_str(&format!("{indent}P.drop({value});\n"));
                    }
                }
                Statement::Assignment(assignment) => {
                    let later_uses = match &assignment.pattern {
                        Pattern::Variable { name, .. } if !is_last => {
                            Some(summarise_uses(name, &statements[index + 1..]))
                        }
                        _ => None,
                    };
                    // Last-use move: a simple binding whose only use is
                    // straight-line in the remaining statements hands its
                    // reference over at that use instead of dup + drop.
                    let movable = matches!(
                        (&assignment.pattern, &assignment.kind),
                        (
                            Pattern::Variable { .. },
                            AssignmentKind::Let | AssignmentKind::Generated
                        )
                    ) && later_uses
                        .is_some_and(|uses| uses.single_straight_line_use());
                    // A raw scalar binding with no later uses has no
                    // scope-exit drop to reference it; zig rejects unused
                    // locals, so emit a discard.
                    let unused_after = later_uses.is_some_and(|uses| uses.count == 0);
                    let (bindings, text, final_value) =
                        self.assignment(assignment, is_last, unused_after, indent);
                    out.push_str(&text);
                    if movable {
                        for binding in &bindings {
                            let _ = self.moved.insert(binding.clone());
                        }
                    } else {
                        own.extend(bindings);
                    }
                    if let Some(final_value) = final_value {
                        // A raw scalar binding boxes on the way out.
                        let final_value = match self.raw_bindings.get(&final_value) {
                            Some(kind) => format!("{}({final_value})", kind.box_helper()),
                            None => final_value,
                        };
                        let drops: Vec<String> =
                            own.iter().chain(pending_drops).cloned().collect();
                        out.push_str(&self.exit_value(&final_value, tail, indent, &drops));
                    }
                }
                Statement::Assert(assert) => {
                    let value = self.scalar(&assert.value, indent);
                    let message = self.panic_message(
                        assert.message.as_ref(),
                        "assertion failed",
                        indent,
                    );
                    let line = self.line_number(&assert.location);
                    out.push_str(&format!(
                        "{indent}if (!({value})) P.gleamPanic({message}, \"{}\", {line});\n",
                        self.module.src_path
                    ));
                    if is_last {
                        let drops: Vec<String> =
                            own.iter().chain(pending_drops).cloned().collect();
                        out.push_str(&self.exit_value("P.NIL", tail, indent, &drops));
                    }
                }
            }
        }

        self.scope = saved_scope;
        out
    }

    /// Render the last statement of a body when it is an expression.
    /// `drops` are all owned bindings live at this point, released before
    /// the exit (after the result is computed).
    fn final_statement(
        &mut self,
        expression: &TypedExpr,
        tail: Tail,
        indent: &str,
        drops: &[String],
    ) -> String {
        if tail == Tail::Return {
            // Constructs containing tail positions recurse; a self call
            // becomes a loop continue.
            match expression {
                TypedExpr::Call { fun, arguments, .. } => {
                    if let Some(rewrite) = self.tail_self_call(fun, arguments, indent, drops) {
                        return rewrite;
                    }
                }
                TypedExpr::Case {
                    subjects, clauses, ..
                } => {
                    let saved_barrier = self.reuse_barrier;
                    self.reuse_barrier += 1;
                    let out = self.case_statement(subjects, clauses, indent, drops);
                    self.reuse_barrier = saved_barrier;
                    return out;
                }
                TypedExpr::Block { statements, .. } => {
                    return self.statements(statements.as_slice(), Tail::Return, indent, drops);
                }
                // noreturn: emit as a bare statement. In a native-ABI
                // function the usual exit would read a union field off a
                // noreturn call, which zig rejects.
                TypedExpr::Panic { .. } | TypedExpr::Todo { .. }
                    if self.native_return.is_some() =>
                {
                    let value = self.expression(expression, indent);
                    return format!("{indent}{value};\n");
                }
                TypedExpr::Pipeline {
                    first_value,
                    assignments,
                    finally,
                    ..
                } => {
                    let saved_scope = self.scope.clone();
                    let mut out = String::new();
                    let steps = self.pipeline_steps(
                        first_value,
                        assignments,
                        &mut out,
                        indent,
                        Some(finally),
                    );
                    let drops: Vec<String> = steps.iter().chain(drops).cloned().collect();
                    out.push_str(&self.final_statement(finally, Tail::Return, indent, &drops));
                    self.scope = saved_scope;
                    return out;
                }
                _ => {}
            }
        }
        let value = self.expression(expression, indent);
        self.exit_value(&value, tail, indent, drops)
    }

    /// Emit an exit (return or labelled break) of `value`, releasing
    /// `drops` after the value is computed. Returns from a native-ABI
    /// function unwrap the boxed value to its raw scalar.
    fn exit_value(&mut self, value: &str, tail: Tail, indent: &str, drops: &[String]) -> String {
        let keyword = match tail {
            Tail::Return => "return",
            Tail::No => BREAK_PLACEHOLDER,
        };
        let unbox = match (tail, self.native_return) {
            (Tail::Return, Some(kind)) => Some(kind),
            _ => None,
        };
        if drops.is_empty() {
            return match unbox {
                Some(kind) => format!("{indent}return ({value}).{};\n", kind.field()),
                None => format!("{indent}{keyword} {value};\n"),
            };
        }
        let result = self.fresh_name("r");
        let mut out = format!("{indent}const {result} = {value};\n");
        for binding in drops {
            out.push_str(&format!("{indent}P.drop({binding});\n"));
        }
        match unbox {
            Some(kind) => {
                out.push_str(&format!("{indent}return ({result}).{};\n", kind.field()))
            }
            None => out.push_str(&format!("{indent}{keyword} {result};\n")),
        }
        out
    }

    /// If this call is a tail call to the enclosing function, rewrite it to
    /// parameter reassignment plus `continue`. The new argument values are
    /// computed first, every live binding (including the old parameter
    /// values) is released, then the parameters are reassigned.
    fn tail_self_call(
        &mut self,
        fun: &TypedExpr,
        arguments: &[CallArg<TypedExpr>],
        indent: &str,
        drops: &[String],
    ) -> Option<String> {
        let (target_name, parameters) = self.tail_target.clone()?;
        let TypedExpr::Var { constructor, .. } = fun else {
            return None;
        };
        let ValueConstructorVariant::ModuleFn { name, module, .. } = &constructor.variant else {
            return None;
        };
        if *name != target_name || *module != self.module.module_name {
            return None;
        }
        self.used_tail_loop = true;
        let mut out = String::new();
        let mut temporaries = Vec::new();
        for argument in arguments.iter() {
            // In a native-ABI function the loop locals are raw scalars.
            let value = if self.native_return.is_some() {
                self.scalar(&argument.value, indent)
            } else {
                self.expression(&argument.value, indent)
            };
            let temporary = self.fresh_name("tail");
            out.push_str(&format!("{indent}const {temporary} = {value};\n"));
            temporaries.push(temporary);
        }
        for binding in drops {
            out.push_str(&format!("{indent}P.drop({binding});\n"));
        }
        for (parameter, temporary) in parameters.iter().zip(&temporaries) {
            out.push_str(&format!("{indent}{parameter} = {temporary};\n"));
        }
        out.push_str(&format!("{indent}continue;\n"));
        Some(out)
    }

    /// Render an assignment. Returns (bindings this created, text,
    /// value to exit with when this is the final statement).
    fn assignment(
        &mut self,
        assignment: &TypedAssignment,
        is_last: bool,
        unused_after: bool,
        indent: &str,
    ) -> (Vec<String>, String, Option<String>) {
        let is_let_assert = matches!(assignment.kind, AssignmentKind::Assert { .. });

        // A simple binding of a scalar-typed value becomes a raw typed
        // local: no reference counting, boxed on demand at later uses.
        if let (Pattern::Variable { name, .. }, false) = (&assignment.pattern, is_let_assert) {
            if let Some(kind) = scalar_kind(&assignment.value.type_()) {
                let raw = self.scalar(&assignment.value, indent);
                let rendered = self.bind(name);
                let _ = self.raw_bindings.insert(rendered.clone(), kind);
                let mut out = format!(
                    "{indent}const {rendered}: {} = {raw};\n",
                    kind.zig_type()
                );
                if unused_after {
                    out.push_str(&format!("{indent}_ = {rendered};\n"));
                }
                let final_value = if is_last { Some(rendered) } else { None };
                return (Vec::new(), out, final_value);
            }
        }

        let value = self.expression(&assignment.value, indent);

        match &assignment.pattern {
            Pattern::Variable { name, .. } if !is_let_assert => {
                let rendered = self.bind(name);
                let out = format!("{indent}const {rendered} = {value};\n");
                if is_last {
                    // The binding is moved out as the result; it is not
                    // registered for a scope-exit drop.
                    (Vec::new(), out, Some(rendered))
                } else {
                    (vec![rendered], out, None)
                }
            }
            Pattern::Discard { .. } if !is_let_assert => {
                if is_last {
                    (Vec::new(), String::new(), Some(value))
                } else {
                    (Vec::new(), format!("{indent}P.drop({value});\n"), None)
                }
            }
            pattern => {
                // Destructuring (and all `let assert`): bind the subject,
                // check the pattern, panic on failure, then bind variables
                // in the enclosing scope.
                let subject = self.fresh_name("subject");
                let mut out = format!("{indent}const {subject} = {value};\n");
                let compiled = self.pattern(pattern, &subject);
                let line = self.line_number(&assignment.location);
                for setup in &compiled.setup {
                    out.push_str(&format!("{indent}{setup}\n"));
                }
                if !compiled.conditions.is_empty() {
                    let condition = compiled.conditions.join(" and ");
                    out.push_str(&format!(
                        "{indent}if (!({condition})) P.gleamPanic(\"pattern match failed\", \"{}\", {line});\n",
                        self.module.src_path
                    ));
                }
                let mut bound = Vec::new();
                for (name, path, owned) in compiled.bindings {
                    let rendered = self.bind(&name);
                    let path = if owned {
                        path
                    } else {
                        format!("P.dup({path})")
                    };
                    out.push_str(&format!("{indent}const {rendered} = {path};\n"));
                    bound.push(rendered);
                }
                if is_last {
                    // The subject is moved out as the result; the pattern
                    // bindings still drop at scope exit.
                    (bound, out, Some(subject))
                } else {
                    out.push_str(&format!("{indent}P.drop({subject});\n"));
                    (bound, out, None)
                }
            }
        }
    }

    fn expression(&mut self, expression: &TypedExpr, indent: &str) -> String {
        match expression {
            TypedExpr::Int { int_value, .. } => format!("P.intValue({int_value})"),

            TypedExpr::Float { value, .. } => format!("P.floatValue({value})"),

            TypedExpr::String { value, .. } => {
                format!("P.copyString(\"{}\")", zig_string_contents(value))
            }

            TypedExpr::Block { statements, .. } => {
                let label = self.next_label("blk");
                let inner_indent = format!("{indent}{INDENT}");
                let body = self.statements(statements.as_slice(), Tail::No, &inner_indent, &[]);
                let body = body.replace(BREAK_PLACEHOLDER, &format!("break :{label}"));
                format!("{label}: {{\n{body}{indent}}}")
            }

            TypedExpr::Pipeline {
                first_value,
                assignments,
                finally,
                ..
            } => {
                let label = self.next_label("blk");
                let inner_indent = format!("{indent}{INDENT}");
                let saved_scope = self.scope.clone();
                let mut out = format!("{label}: {{\n");
                let steps = self.pipeline_steps(
                    first_value,
                    assignments,
                    &mut out,
                    &inner_indent,
                    Some(finally),
                );
                let finally = self.expression(finally, &inner_indent);
                let result = self.fresh_name("r");
                out.push_str(&format!("{inner_indent}const {result} = {finally};\n"));
                for step in &steps {
                    out.push_str(&format!("{inner_indent}P.drop({step});\n"));
                }
                out.push_str(&format!("{inner_indent}break :{label} {result};\n"));
                out.push_str(&format!("{indent}}}"));
                self.scope = saved_scope;
                out
            }

            TypedExpr::Var {
                constructor, name, ..
            } => self.variable(name, &constructor.variant),

            TypedExpr::Call { fun, arguments, .. } => self.call(fun, arguments, indent),

            TypedExpr::BinOp {
                operator,
                left,
                right,
                ..
            } => match operator {
                // String concatenation, and structural equality on boxed
                // operands, stay on the consuming boxed helpers.
                BinOp::Concatenate => {
                    let left = self.expression(left, indent);
                    let right = self.expression(right, indent);
                    binary_operator(*operator, &left, &right)
                }
                BinOp::Eq | BinOp::NotEq if scalar_kind(&left.type_()).is_none() => {
                    let left = self.expression(left, indent);
                    let right = self.expression(right, indent);
                    binary_operator(*operator, &left, &right)
                }
                // Everything else computes raw and boxes the result once.
                _ => {
                    let kind = scalar_kind(&expression.type_())
                        .expect("zig codegen: non-scalar arithmetic result");
                    format!("{}({})", kind.box_helper(), self.scalar(expression, indent))
                }
            },

            TypedExpr::Case {
                subjects, clauses, ..
            } => {
                // Conditional region: pending reuse tokens must not be
                // consumed by constructions inside clause bodies. The level
                // restores afterwards so later siblings can still consume.
                let saved_barrier = self.reuse_barrier;
                self.reuse_barrier += 1;
                let label = self.next_label("case");
                let inner_indent = format!("{indent}{INDENT}");
                let body =
                    self.case_clauses(subjects, clauses, Tail::No, &label, &inner_indent, &[]);
                self.reuse_barrier = saved_barrier;
                format!("{label}: {{\n{body}{inner_indent}unreachable;\n{indent}}}")
            }

            TypedExpr::Tuple { elements, .. } => {
                let count = elements.len();
                let elements = elements
                    .iter()
                    .map(|element| self.expression(element, indent))
                    .join(", ");
                // A pending same-arity tuple token: overwrite the matched
                // tuple's element slice in place.
                if self
                    .reuse_token
                    .as_ref()
                    .is_some_and(|(_, kind, armed)| {
                        *kind == ReuseKind::Tuple(count) && *armed == self.reuse_barrier
                    })
                {
                    let (token, _, _) = self.reuse_token.take().expect("checked");
                    return format!(
                        "P.tupleReuse({token}, &[_]Value{{ {elements} }})"
                    );
                }
                format!("P.tupleValue(&[_]Value{{ {elements} }})")
            }

            TypedExpr::TupleIndex { tuple, index, .. } => {
                // A live local container: borrow the element and take one
                // reference on it directly, skipping the container
                // dup/drop pair that P.tupleField(P.dup(v)) would cost.
                match self.borrowable_local(tuple) {
                    Some(container) => {
                        format!("P.dup(({container}).tuple[{index}])")
                    }
                    None => {
                        format!("P.tupleField({}, {index})", self.expression(tuple, indent))
                    }
                }
            }

            TypedExpr::List { elements, tail, .. } => {
                // A pending reuse token feeds the canonical `[x, ..rest]`
                // construction: the matched cell is written in place when
                // it was unshared.
                if elements.len() == 1
                    && tail.is_some()
                    && self
                        .reuse_token
                        .as_ref()
                        .is_some_and(|(_, kind, armed)| {
                            *kind == ReuseKind::Cons && *armed == self.reuse_barrier
                        })
                {
                    let (token, _, _) = self.reuse_token.take().expect("checked");
                    let head = self.expression(&elements[0], indent);
                    let tail = self.expression(tail.as_ref().expect("tail"), indent);
                    return format!("P.consReuse({token}, {head}, {tail})");
                }
                let tail = match tail {
                    Some(tail) => self.expression(tail, indent),
                    None => "P.emptyList()".to_string(),
                };
                if elements.is_empty() {
                    return tail;
                }
                let elements = elements
                    .iter()
                    .map(|element| self.expression(element, indent))
                    .join(", ");
                format!("P.listFromSlice(&[_]Value{{ {elements} }}, {tail})")
            }

            TypedExpr::RecordAccess { record, index, .. } => {
                match self.borrowable_local(record) {
                    Some(container) => {
                        format!("P.dup(({container}).record.fields[{index}])")
                    }
                    None => {
                        format!("P.recordField({}, {index})", self.expression(record, indent))
                    }
                }
            }

            TypedExpr::PositionalAccess { record, index, .. } => {
                match self.borrowable_local(record) {
                    Some(container) => {
                        format!("P.dup(({container}).record.fields[{index}])")
                    }
                    None => {
                        format!("P.recordField({}, {index})", self.expression(record, indent))
                    }
                }
            }

            TypedExpr::RecordUpdate {
                updated_record,
                updated_record_assigned_name,
                constructor,
                arguments,
                ..
            } => {
                // The typer fills `arguments` with explicit values plus
                // implicit accesses to the updated record, referencing it by
                // `updated_record_assigned_name` when it is not a variable.
                match updated_record_assigned_name {
                    Some(name) => {
                        let label = self.next_label("blk");
                        let inner_indent = format!("{indent}{INDENT}");
                        let saved_scope = self.scope.clone();
                        let record = self.expression(updated_record, &inner_indent);
                        let rendered = self.bind(name);
                        let call = self.call(constructor, arguments, &inner_indent);
                        let result = self.fresh_name("r");
                        self.scope = saved_scope;
                        format!(
                            "{label}: {{\n{inner_indent}const {rendered} = {record};\n{inner_indent}const {result} = {call};\n{inner_indent}P.drop({rendered});\n{inner_indent}break :{label} {result};\n{indent}}}"
                        )
                    }
                    None => self.call(constructor, arguments, indent),
                }
            }

            TypedExpr::Fn {
                arguments, body, ..
            } => self.anonymous_function(
                arguments.iter().map(|argument| {
                    argument
                        .names
                        .get_variable_name()
                        .cloned()
                        .unwrap_or("_".into())
                }),
                body.as_slice(),
            ),

            TypedExpr::ModuleSelect {
                module_name,
                constructor,
                ..
            } => match constructor {
                ModuleValueConstructor::Fn { module, name, .. } => {
                    self.function_reference(module, name, function_arity(expression))
                }
                ModuleValueConstructor::Record {
                    name,
                    arity,
                    field_map,
                    ..
                } => {
                    if *arity == 0 {
                        self.record_construction(&module_name.clone(), &name.clone(), &[], None)
                    } else {
                        self.constructor_reference(
                            &module_name.clone(),
                            &name.clone(),
                            *arity as usize,
                            field_map.clone().as_ref(),
                        )
                    }
                }
                ModuleValueConstructor::Constant { .. } => {
                    let label = match expression {
                        TypedExpr::ModuleSelect { label, .. } => label.clone(),
                        _ => unreachable!("constant is only reachable via module select"),
                    };
                    if *module_name == self.module.module_name {
                        format!("{}()", constant_identifier(&label))
                    } else {
                        let _ = self.module.modules_used.insert(module_name.clone());
                        format!(
                            "{}.{}()",
                            module_ref(module_name),
                            constant_identifier(&label)
                        )
                    }
                }
            },

            TypedExpr::Echo {
                expression: inner,
                location,
                ..
            } => {
                let value = match inner {
                    Some(inner) => self.expression(inner, indent),
                    // `|> echo` with no argument: echo the pipe value. The
                    // pipe binding is still dropped at pipeline exit, so
                    // this use takes its own reference.
                    None => format!(
                        "P.dup({})",
                        self.pipe_value
                            .clone()
                            .expect("zig codegen: echo with no expression outside a pipeline")
                    ),
                };
                let line = self.line_number(location);
                format!("P.echo({value}, \"{}\", {line})", self.module.src_path)
            }

            TypedExpr::Panic {
                message, location, ..
            } => {
                let message =
                    self.panic_message(message.as_deref(), "panic expression evaluated", indent);
                let line = self.line_number(location);
                format!(
                    "P.gleamPanic({message}, \"{}\", {line})",
                    self.module.src_path
                )
            }

            TypedExpr::Todo {
                message, location, ..
            } => {
                let message =
                    self.panic_message(message.as_deref(), "todo expression evaluated", indent);
                let line = self.line_number(location);
                format!(
                    "P.gleamPanic({message}, \"{}\", {line})",
                    self.module.src_path
                )
            }

            TypedExpr::NegateInt { .. } => {
                format!("P.intValue({})", self.scalar(expression, indent))
            }

            TypedExpr::NegateBool { .. } => {
                format!("P.boolValue({})", self.scalar(expression, indent))
            }

            TypedExpr::BitArray { segments, .. } => {
                self.bit_array_construction(segments, indent)
            }

            TypedExpr::Invalid { .. } => {
                panic!("zig codegen: invalid expression reached codegen")
            }
        }
    }

    /// A container expression that can be field-read in place: a local
    /// variable that is not move-approved (a moved var must go through
    /// the consuming path or its reference would leak). The variable
    /// stays live until its scope-exit drop, so borrowing a field out of
    /// it needs no dup/drop pair on the container.
    fn borrowable_local(&self, expression: &TypedExpr) -> Option<String> {
        let TypedExpr::Var {
            constructor, name, ..
        } = expression
        else {
            return None;
        };
        if !matches!(
            constructor.variant,
            ValueConstructorVariant::LocalVariable { .. }
        ) {
            return None;
        }
        let rendered = self.scope.get(name)?;
        if self.moved.contains(rendered) {
            return None;
        }
        Some(rendered.clone())
    }

    /// Render a scalar-typed (Int/Float/Bool) expression as a raw zig
    /// i64/f64/bool. Total: subtrees without a raw form render boxed and
    /// read the union field, which is free for scalars (no references).
    fn scalar(&mut self, expression: &TypedExpr, indent: &str) -> String {
        let kind = scalar_kind(&expression.type_())
            .expect("zig codegen: scalar render of a non-scalar expression");
        match expression {
            TypedExpr::Int { int_value, .. } => format!("{int_value}"),

            TypedExpr::Float { value, .. } => value.to_string(),

            TypedExpr::Var {
                constructor, name, ..
            } => match &constructor.variant {
                ValueConstructorVariant::LocalVariable { .. } => {
                    let rendered = self.scope.get(name).cloned().unwrap_or_else(|| {
                        panic!("zig codegen: variable {name} not in scope")
                    });
                    if self.raw_bindings.contains_key(&rendered) {
                        rendered
                    } else {
                        // Scalars carry no references: read the field
                        // directly, no dup or move bookkeeping needed.
                        format!("({rendered}).{}", kind.field())
                    }
                }
                ValueConstructorVariant::Record { name, .. } if name == "True" => {
                    "true".to_string()
                }
                ValueConstructorVariant::Record { name, .. } if name == "False" => {
                    "false".to_string()
                }
                _ => format!("({}).{}", self.expression(expression, indent), kind.field()),
            },

            TypedExpr::BinOp {
                operator,
                left,
                right,
                ..
            } => self.scalar_binop(*operator, left, right, indent),

            TypedExpr::NegateInt { value, .. } => {
                format!("(0 -% {})", self.scalar(value, indent))
            }

            TypedExpr::NegateBool { value, .. } => {
                format!("!({})", self.scalar(value, indent))
            }

            TypedExpr::Call { fun, arguments, .. } => {
                match self.native_call(fun, arguments, indent) {
                    Some((call, _)) => call,
                    None => {
                        format!("({}).{}", self.expression(expression, indent), kind.field())
                    }
                }
            }

            // noreturn coerces to any scalar type; there is no field to
            // read.
            TypedExpr::Panic { .. } | TypedExpr::Todo { .. } => {
                self.expression(expression, indent)
            }

            // A scalar field of a live local: borrow it in place. No
            // dup/drop on the container, no Value round-trip — the hot
            // shape of record-heavy numeric code (vec.x +. vec.y).
            TypedExpr::RecordAccess { record, index, .. } => {
                match self.borrowable_local(record) {
                    Some(container) => format!(
                        "(({container}).record.fields[{index}]).{}",
                        kind.field()
                    ),
                    None => {
                        format!("({}).{}", self.expression(expression, indent), kind.field())
                    }
                }
            }
            TypedExpr::PositionalAccess { record, index, .. } => {
                match self.borrowable_local(record) {
                    Some(container) => format!(
                        "(({container}).record.fields[{index}]).{}",
                        kind.field()
                    ),
                    None => {
                        format!("({}).{}", self.expression(expression, indent), kind.field())
                    }
                }
            }
            TypedExpr::TupleIndex { tuple, index, .. } => {
                match self.borrowable_local(tuple) {
                    Some(container) => {
                        format!("(({container}).tuple[{index}]).{}", kind.field())
                    }
                    None => {
                        format!("({}).{}", self.expression(expression, indent), kind.field())
                    }
                }
            }

            _ => format!("({}).{}", self.expression(expression, indent), kind.field()),
        }
    }

    fn scalar_binop(
        &mut self,
        operator: BinOp,
        left: &TypedExpr,
        right: &TypedExpr,
        indent: &str,
    ) -> String {
        let token = match operator {
            BinOp::And | BinOp::Or => {
                let left = self.scalar(left, indent);
                // The right operand may not run: a conditional region for
                // reuse tokens, exactly as in the boxed path.
                let saved_barrier = self.reuse_barrier;
                self.reuse_barrier += 1;
                let right = self.scalar(right, indent);
                self.reuse_barrier = saved_barrier;
                let keyword = if operator == BinOp::And { "and" } else { "or" };
                return format!("({left} {keyword} {right})");
            }
            BinOp::Eq | BinOp::NotEq => {
                if scalar_kind(&left.type_()).is_some() {
                    let left = self.scalar(left, indent);
                    let right = self.scalar(right, indent);
                    let token = if operator == BinOp::Eq { "==" } else { "!=" };
                    return format!("({left} {token} {right})");
                }
                // Structural equality on boxed operands.
                let left = self.expression(left, indent);
                let right = self.expression(right, indent);
                let helper = if operator == BinOp::Eq { "eq" } else { "notEq" };
                return format!("(P.{helper}({left}, {right})).bool");
            }
            // Division and remainder keep Gleam's zero-divisor semantics.
            BinOp::DivInt => {
                let left = self.scalar(left, indent);
                let right = self.scalar(right, indent);
                return format!("P.rawDivInt({left}, {right})");
            }
            BinOp::RemainderInt => {
                let left = self.scalar(left, indent);
                let right = self.scalar(right, indent);
                return format!("P.rawRemInt({left}, {right})");
            }
            BinOp::DivFloat => {
                let left = self.scalar(left, indent);
                let right = self.scalar(right, indent);
                return format!("P.rawDivFloat({left}, {right})");
            }
            BinOp::AddInt => "+%",
            BinOp::SubInt => "-%",
            BinOp::MultInt => "*%",
            BinOp::AddFloat => "+",
            BinOp::SubFloat => "-",
            BinOp::MultFloat => "*",
            BinOp::LtInt | BinOp::LtFloat => "<",
            BinOp::LtEqInt | BinOp::LtEqFloat => "<=",
            BinOp::GtInt | BinOp::GtFloat => ">",
            BinOp::GtEqInt | BinOp::GtEqFloat => ">=",
            BinOp::Concatenate => {
                unreachable!("zig codegen: concatenate is not a scalar operation")
            }
        };
        let left = self.scalar(left, indent);
        let right = self.scalar(right, indent);
        format!("({left} {token} {right})")
    }

    /// A call to a same-module function with the raw scalar ABI: render it
    /// natively (raw arguments, raw result) and return the call with its
    /// result kind.
    fn native_call(
        &mut self,
        fun: &TypedExpr,
        arguments: &[CallArg<TypedExpr>],
        indent: &str,
    ) -> Option<(String, ScalarKind)> {
        let (module, name) = match fun {
            TypedExpr::Var { constructor, .. } => match &constructor.variant {
                ValueConstructorVariant::ModuleFn { name, module, .. } => {
                    (module.clone(), name.clone())
                }
                _ => return None,
            },
            TypedExpr::ModuleSelect {
                constructor: ModuleValueConstructor::Fn { module, name, .. },
                ..
            } => (module.clone(), name.clone()),
            _ => return None,
        };
        if module != self.module.module_name {
            return None;
        }
        let (_, return_kind) = self.module.native_signatures.get(&name)?.clone();
        let rendered = arguments
            .iter()
            .map(|argument| self.scalar(&argument.value, indent))
            .join(", ");
        Some((
            format!("{}({rendered})", zig_identifier(&format!("native${name}"))),
            return_kind,
        ))
    }

    /// A call to a same-module function with the borrowed ABI. Borrowed
    /// argument positions pass a reference without taking one: a live
    /// local goes in directly, a scalar-typed pure operand boxes inline,
    /// and anything else is bound to a temporary that the caller drops
    /// after the call. When temporaries are needed, every effectful
    /// argument is bound in order so left-to-right evaluation holds.
    fn borrowed_call(
        &mut self,
        fun: &TypedExpr,
        arguments: &[CallArg<TypedExpr>],
        indent: &str,
    ) -> Option<String> {
        let (module, name) = match fun {
            TypedExpr::Var { constructor, .. } => match &constructor.variant {
                ValueConstructorVariant::ModuleFn { name, module, .. } => {
                    (module.clone(), name.clone())
                }
                _ => return None,
            },
            TypedExpr::ModuleSelect {
                constructor: ModuleValueConstructor::Fn { module, name, .. },
                ..
            } => (module.clone(), name.clone()),
            _ => return None,
        };
        if module != self.module.module_name {
            return None;
        }
        let flags = self.module.borrowed_signatures.get(&name)?.clone();
        if flags.len() != arguments.len() {
            return None;
        }
        let borrowed_name = zig_identifier(&format!("borrowed${name}"));

        // Pure operands are order-insensitive; everything else must bind
        // to a temporary in argument order.
        fn is_pure_operand(expression: &TypedExpr) -> bool {
            matches!(
                expression,
                TypedExpr::Var { .. } | TypedExpr::Int { .. } | TypedExpr::Float { .. }
            )
        }

        enum Rendered {
            /// Passed inline; no ownership handed over, nothing to drop.
            Direct(String),
            /// Bound to a temporary in order; dropped after the call when
            /// the flag is set (borrowed boxed argument).
            Temp(String, String, bool),
        }
        let mut slots = Vec::new();
        for (argument, borrowed) in arguments.iter().zip(&flags) {
            if *borrowed {
                if let Some(local) = self.borrowable_local(&argument.value) {
                    if let Some(kind) = self.raw_bindings.get(&local) {
                        // A raw scalar local boxes inline; no references.
                        slots.push(Rendered::Direct(format!(
                            "{}({local})",
                            kind.box_helper()
                        )));
                    } else {
                        slots.push(Rendered::Direct(local));
                    }
                    continue;
                }
                let scalar = scalar_kind(&argument.value.type_());
                if let (Some(kind), true) = (scalar, is_pure_operand(&argument.value)) {
                    let raw = self.scalar(&argument.value, indent);
                    slots.push(Rendered::Direct(format!("{}({raw})", kind.box_helper())));
                    continue;
                }
                let value = self.expression(&argument.value, indent);
                let temp = self.fresh_name("bor");
                // Scalar temporaries carry no reference to release.
                slots.push(Rendered::Temp(temp, value, scalar.is_none()));
            } else if is_pure_operand(&argument.value) {
                slots.push(Rendered::Direct(self.expression(&argument.value, indent)));
            } else {
                let value = self.expression(&argument.value, indent);
                let temp = self.fresh_name("own");
                // Owned: ownership flows through the temporary into the
                // callee; nothing to drop here.
                slots.push(Rendered::Temp(temp, value, false));
            }
        }

        let needs_block = slots
            .iter()
            .any(|slot| matches!(slot, Rendered::Temp(..)));
        let rendered_arguments = slots
            .iter()
            .map(|slot| match slot {
                Rendered::Direct(text) => text.clone(),
                Rendered::Temp(temp, _, _) => temp.clone(),
            })
            .join(", ");
        if !needs_block {
            return Some(format!("{borrowed_name}({rendered_arguments})"));
        }
        let label = self.next_label("bc");
        let inner_indent = format!("{indent}{INDENT}");
        let mut out = format!("{label}: {{\n");
        for slot in &slots {
            if let Rendered::Temp(temp, value, _) = slot {
                out.push_str(&format!("{inner_indent}const {temp} = {value};\n"));
            }
        }
        let result = self.fresh_name("r");
        out.push_str(&format!(
            "{inner_indent}const {result} = {borrowed_name}({rendered_arguments});\n"
        ));
        for slot in &slots {
            if let Rendered::Temp(temp, _, true) = slot {
                out.push_str(&format!("{inner_indent}P.drop({temp});\n"));
            }
        }
        out.push_str(&format!("{inner_indent}break :{label} {result};\n{indent}}}"));
        Some(out)
    }

    /// Render pipeline step bindings into `out`, returning the rendered
    /// binding identifiers the caller must drop at pipeline exit. Steps
    /// whose value is consumed exactly once by the following step (the
    /// overwhelmingly common case) are move-optimised and excluded.
    fn pipeline_steps(
        &mut self,
        first_value: &crate::ast::TypedPipelineAssignment,
        assignments: &[(crate::ast::TypedPipelineAssignment, crate::ast::PipelineAssignmentKind)],
        out: &mut String,
        indent: &str,
        finally: Option<&TypedExpr>,
    ) -> Vec<String> {
        // Later steps and the final expression, in evaluation order, for
        // the per-step last-use scan.
        let step_values: Vec<&TypedExpr> = assignments
            .iter()
            .map(|(assignment, _)| &*assignment.value)
            .collect();
        let step_names: Vec<&EcoString> =
            assignments.iter().map(|(assignment, _)| &assignment.name).collect();

        // Steps all share the source name "_pipe": the scan stops at the
        // next rebind of the same name, so only the immediately following
        // step (and the finale, when nothing rebinds in between) counts.
        let step_is_moved = |name: &EcoString, from: usize| -> bool {
            let mut summary = UseSummary::default();
            for (value, later_name) in step_values[from..]
                .iter()
                .zip(&step_names[from..])
            {
                expression_uses(name, value, false, &mut summary);
                if *later_name == name {
                    return summary.single_straight_line_use();
                }
            }
            if let Some(finally) = finally {
                expression_uses(name, finally, false, &mut summary);
            }
            summary.single_straight_line_use()
        };

        let first = self.expression(&first_value.value, indent);
        let mut previous = self.bind(&first_value.name);
        let mut bindings = Vec::new();
        if step_is_moved(&first_value.name, 0) {
            let _ = self.moved.insert(previous.clone());
        } else {
            bindings.push(previous.clone());
        }
        out.push_str(&format!("{indent}const {previous} = {first};\n"));
        for (index, (assignment, _kind)) in assignments.iter().enumerate() {
            // A bare `|> echo` step has no expression: it echoes the value
            // flowing through the pipe.
            self.pipe_value = Some(previous.clone());
            let value = self.expression(&assignment.value, indent);
            self.pipe_value = None;
            let rendered = self.bind(&assignment.name);
            out.push_str(&format!("{indent}const {rendered} = {value};\n"));
            if step_is_moved(&assignment.name, index + 1) {
                let _ = self.moved.insert(rendered.clone());
            } else {
                bindings.push(rendered.clone());
            }
            previous = rendered;
        }
        self.pipe_value = Some(previous);
        bindings
    }

    fn call(
        &mut self,
        fun: &TypedExpr,
        arguments: &[CallArg<TypedExpr>],
        indent: &str,
    ) -> String {
        // Same-module native-ABI callee: raw call, box the result.
        if let Some((call, return_kind)) = self.native_call(fun, arguments, indent) {
            return format!("{}({call})", return_kind.box_helper());
        }

        // Same-module borrowed-ABI callee: borrowed arguments pass
        // without taking a reference.
        if let Some(call) = self.borrowed_call(fun, arguments, indent) {
            return call;
        }

        let rendered_arguments = arguments
            .iter()
            .map(|argument| self.expression(&argument.value, indent))
            .collect::<Vec<_>>();

        // Direct calls to known functions and constructors.
        match fun {
            TypedExpr::Var { constructor, .. } => match &constructor.variant {
                ValueConstructorVariant::ModuleFn { name, module, .. } => {
                    let target = self.module_function_target(module, name);
                    return format!("{target}({})", rendered_arguments.join(", "));
                }
                ValueConstructorVariant::Record {
                    name,
                    module,
                    field_map,
                    ..
                } => {
                    return self.record_construction(
                        &module.clone(),
                        &name.clone(),
                        &rendered_arguments,
                        field_map.clone().as_ref(),
                    );
                }
                _ => {}
            },
            TypedExpr::ModuleSelect {
                module_name,
                constructor,
                ..
            } => match constructor {
                ModuleValueConstructor::Fn { module, name, .. } => {
                    let target = self.module_function_target(module, name);
                    return format!("{target}({})", rendered_arguments.join(", "));
                }
                ModuleValueConstructor::Record {
                    name, field_map, ..
                } => {
                    return self.record_construction(
                        &module_name.clone(),
                        &name.clone(),
                        &rendered_arguments,
                        field_map.clone().as_ref(),
                    );
                }
                _ => {}
            },
            _ => {}
        }

        // Anything else is a closure value.
        let closure = self.expression(fun, indent);
        let arity = rendered_arguments.len();
        if arity > 6 {
            panic!("zig codegen: closure calls with more than 6 arguments are not supported yet");
        }
        if rendered_arguments.is_empty() {
            format!("P.call0({closure})")
        } else {
            format!("P.call{arity}({closure}, {})", rendered_arguments.join(", "))
        }
    }

    fn module_function_target(&mut self, module: &EcoString, name: &EcoString) -> String {
        if *module == self.module.module_name {
            zig_identifier(name)
        } else {
            let _ = self.module.modules_used.insert(module.clone());
            format!("{}.{}", module_ref(module), zig_identifier(name))
        }
    }

    fn record_construction(
        &mut self,
        module: &EcoString,
        name: &EcoString,
        arguments: &[String],
        field_map: Option<&crate::type_::FieldMap>,
    ) -> String {
        if *module == PRELUDE_MODULE_NAME {
            match name.as_str() {
                "True" => return "P.TRUE".to_string(),
                "False" => return "P.FALSE".to_string(),
                "Nil" => return "P.NIL".to_string(),
                // Ok and Error are plain records.
                _ => {}
            }
        }
        if arguments.is_empty() {
            return format!("P.makeRecord(\"{name}\", &[_]Value{{}})");
        }
        let labels = field_map.map(|field_map| {
            let mut labels: Vec<Option<&EcoString>> = vec![None; field_map.arity as usize];
            for (label, index) in &field_map.fields {
                labels[*index as usize] = Some(label);
            }
            labels
                .iter()
                .map(|label| match label {
                    Some(label) => format!("\"{label}\""),
                    None => "null".to_string(),
                })
                .join(", ")
        });
        let labels = match labels {
            Some(labels) => format!("&[_]?[]const u8{{ {labels} }}"),
            None => "&[_]?[]const u8{}".to_string(),
        };
        // A pending same-arity record token: overwrite the matched
        // record's allocation (struct + field slice) in place. The
        // variant name and labels are static strings, so retagging to a
        // different variant of the type is free.
        if self
            .reuse_token
            .as_ref()
            .is_some_and(|(_, kind, armed)| {
                *kind == ReuseKind::Record(arguments.len()) && *armed == self.reuse_barrier
            })
        {
            let (token, _, _) = self.reuse_token.take().expect("checked");
            return format!(
                "P.makeRecordReuse({token}, \"{name}\", &[_]Value{{ {} }}, {labels})",
                arguments.join(", ")
            );
        }
        format!(
            "P.makeRecordL(\"{name}\", &[_]Value{{ {} }}, {labels})",
            arguments.join(", ")
        )
    }

    /// A module function used as a value: wrap it in a lifted closure fn.
    fn function_reference(
        &mut self,
        module: &EcoString,
        name: &EcoString,
        arity: usize,
    ) -> String {
        let key = (module.clone(), name.clone(), arity);
        if let Some(wrapper) = self.module.wrapper_cache.get(&key) {
            return format!("P.makeClosure(@ptrCast(&{wrapper}), &[_]Value{{}})");
        }
        let target = self.module_function_target(module, name);
        let wrapper = zig_identifier(&format!(
            "wrap${}${name}",
            module.as_str().replace('/', "$")
        ));
        let parameters = (0..arity)
            .map(|index| format!("{}: Value", zig_identifier(&format!("p${index}"))))
            .join(", ");
        let forwarded = (0..arity)
            .map(|index| zig_identifier(&format!("p${index}")))
            .join(", ");
        let separator = if arity == 0 { "" } else { ", " };
        self.module.lifted.push(format!(
            "fn {wrapper}(@\"env$\": []const Value{separator}{parameters}) Value {{\n{INDENT}_ = @\"env$\";\n{INDENT}return {target}({forwarded});\n}}\n"
        ));
        let _ = self.module.wrapper_cache.insert(key, wrapper.clone());
        format!("P.makeClosure(@ptrCast(&{wrapper}), &[_]Value{{}})")
    }

    /// A record constructor used as a value: wrap it in a lifted closure fn.
    fn constructor_reference(
        &mut self,
        module: &EcoString,
        name: &EcoString,
        arity: usize,
        field_map: Option<&crate::type_::FieldMap>,
    ) -> String {
        let key = (module.clone(), EcoString::from(format!("constructor#{name}")), arity);
        if let Some(wrapper) = self.module.wrapper_cache.get(&key) {
            return format!("P.makeClosure(@ptrCast(&{wrapper}), &[_]Value{{}})");
        }
        let wrapper = zig_identifier(&format!("wrapc${name}${arity}"));
        let parameters = (0..arity)
            .map(|index| format!("{}: Value", zig_identifier(&format!("p${index}"))))
            .join(", ");
        let forwarded = (0..arity)
            .map(|index| zig_identifier(&format!("p${index}")))
            .collect::<Vec<_>>();
        // The wrapper body renders through this generator but lives in a
        // lifted fn: a pending reuse token must not be consumed there
        // (its identifier is not in the wrapper's scope).
        let stashed_token = self.reuse_token.take();
        let construction = self.record_construction(module, name, &forwarded, field_map);
        self.reuse_token = stashed_token;
        self.module.lifted.push(format!(
            "fn {wrapper}(@\"env$\": []const Value, {parameters}) Value {{\n{INDENT}_ = @\"env$\";\n{INDENT}return {construction};\n}}\n"
        ));
        let _ = self.module.wrapper_cache.insert(key, wrapper.clone());
        format!("P.makeClosure(@ptrCast(&{wrapper}), &[_]Value{{}})")
    }

    /// Lift an anonymous function to a module-level fn taking its captured
    /// environment as a leading slice parameter.
    fn anonymous_function(
        &mut self,
        parameter_names: impl Iterator<Item = EcoString>,
        body: &[TypedStatement],
    ) -> String {
        let lambda_index = self.module.lambda_counter;
        self.module.lambda_counter += 1;
        let lambda = zig_identifier(&format!("lambda${lambda_index}"));

        // Captures: every variable the body references from the enclosing
        // scope. Free-variable analysis: names bound in the current scope
        // that appear in the body.
        let free = free_variables(body);
        let mut captures: Vec<(EcoString, String)> = Vec::new();
        for name in free {
            if let Some(rendered) = self.scope.get(&name) {
                captures.push((name, rendered.clone()));
            }
        }

        // Lambda bodies run zero or many times; a fresh generator means a
        // fresh (empty) token state, so no cross-contamination is possible.
        let mut generator = FunctionGenerator::new(self.module);
        for (index, (name, _)) in captures.iter().enumerate() {
            let _ = generator
                .scope
                .insert(name.clone(), format!("@\"env$\"[{index}]"));
        }
        let mut parameter_list = vec!["@\"env$\": []const Value".to_string()];
        let mut dropped_params = Vec::new();
        for parameter_name in parameter_names {
            let rendered = generator.bind(&parameter_name);
            parameter_list.push(format!("{rendered}: Value"));
            if summarise_uses(&parameter_name, body).single_straight_line_use() {
                let _ = generator.moved.insert(rendered);
            } else {
                dropped_params.push(rendered);
            }
        }
        // Parameters are owned and released at exit; the env is borrowed
        // (it belongs to the closure and is released with the closure).
        let body_text = generator.statements(body, Tail::Return, INDENT, &dropped_params);

        let mut discards = String::new();
        if !body_text.contains("@\"env$\"[") {
            discards.push_str(&format!("{INDENT}_ = @\"env$\";\n"));
        }

        self.module.lifted.push(format!(
            "fn {lambda}({}) Value {{\n{discards}{body_text}}}\n",
            parameter_list.join(", ")
        ));

        // Creating the closure takes a reference to each captured value;
        // raw scalar captures box into the environment.
        let environment = captures
            .iter()
            .map(|(_, rendered)| match self.raw_bindings.get(rendered) {
                Some(kind) => format!("{}({rendered})", kind.box_helper()),
                None => format!("P.dup({rendered})"),
            })
            .join(", ");
        format!("P.makeClosure(@ptrCast(&{lambda}), &[_]Value{{ {environment} }})")
    }

    /// A case in tail/statement position: clause bodies return directly,
    /// releasing clause bindings, subjects and all enclosing live bindings
    /// (`drops`) before each exit.
    fn case_statement(
        &mut self,
        subjects: &[TypedExpr],
        clauses: &[TypedClause],
        indent: &str,
        drops: &[String],
    ) -> String {
        let body = self.case_clauses(subjects, clauses, Tail::Return, "", indent, drops);
        format!("{body}{indent}unreachable;\n")
    }

    fn case_clauses(
        &mut self,
        subjects: &[TypedExpr],
        clauses: &[TypedClause],
        tail: Tail,
        value_label: &str,
        indent: &str,
        pending_drops: &[String],
    ) -> String {
        let mut out = String::new();
        let mut subject_names = Vec::new();
        for subject in subjects {
            let value = self.expression(subject, indent);
            let rendered = self.fresh_name("s");
            out.push_str(&format!("{indent}const {rendered} = {value};\n"));
            subject_names.push(rendered);
        }

        for clause in clauses {
            let mut multi_patterns = vec![&clause.pattern];
            multi_patterns.extend(clause.alternative_patterns.iter());

            for multi_pattern in multi_patterns {
                let clause_label = self.next_label("c");
                let inner_indent = format!("{indent}{INDENT}");
                let saved_scope = self.scope.clone();

                let mut setup = Vec::new();
                let mut conditions = Vec::new();
                let mut bindings = Vec::new();
                for (pattern, subject) in multi_pattern.iter().zip(&subject_names) {
                    let compiled = self.pattern(pattern, subject);
                    setup.extend(compiled.setup);
                    conditions.extend(compiled.conditions);
                    bindings.extend(compiled.bindings);
                }

                let mut clause_body = String::new();
                for line in &setup {
                    clause_body.push_str(&format!("{inner_indent}{line}\n"));
                }
                if !conditions.is_empty() {
                    clause_body.push_str(&format!(
                        "{inner_indent}if (!({})) break :{clause_label};\n",
                        conditions.join(" and ")
                    ));
                }
                let mut bound = Vec::new();
                let mut binding_text = String::new();
                for (name, path, owned) in bindings {
                    let rendered = self.bind(&name);
                    let path = if owned {
                        path
                    } else {
                        format!("P.dup({path})")
                    };
                    binding_text
                        .push_str(&format!("{inner_indent}const {rendered} = {path};\n"));
                    bound.push(rendered);
                }

                let mut guard_and_body = String::new();
                if let Some(guard) = &clause.guard {
                    let guard = self.guard(guard);
                    // A failed guard falls through to the next clause; the
                    // clause bindings it took must be released first.
                    let mut on_fail = String::new();
                    for binding in &bound {
                        on_fail.push_str(&format!("P.drop({binding}); "));
                    }
                    guard_and_body.push_str(&format!(
                        "{inner_indent}if (!(({guard}).bool)) {{ {on_fail}break :{clause_label}; }}\n"
                    ));
                }
                // FBIP reuse: hand the matched allocation (cons cell,
                // record, or tuple) to the body's guaranteed same-shape
                // construction. Extracted only after the guard has passed
                // (a failed guard leaves the subject owned by the case),
                // and the subject is then excluded from this clause's
                // exit drops.
                let reuse_kind = if subjects.len() == 1 && self.reuse_token.is_none() {
                    clause_reuse_kind(clause)
                } else {
                    None
                };
                if let Some(kind) = reuse_kind {
                    let token = self.fresh_name("reuse");
                    let arm = match kind {
                        ReuseKind::Cons => {
                            format!("P.dropReuseCons({})", subject_names[0])
                        }
                        ReuseKind::Record(arity) => {
                            format!("P.dropReuseRecord({}, {arity})", subject_names[0])
                        }
                        ReuseKind::Tuple(arity) => {
                            format!("P.dropReuseTuple({}, {arity})", subject_names[0])
                        }
                    };
                    guard_and_body
                        .push_str(&format!("{inner_indent}const {token} = {arm};\n"));
                    self.reuse_token = Some((token, kind, self.reuse_barrier));
                }
                let subject_drops: &[String] =
                    if reuse_kind.is_some() { &[] } else { &subject_names };

                // Everything owned at the exit taken from this clause:
                // clause bindings, the case subjects, and (in tail position)
                // the enclosing scope's live bindings.
                let exit_drops: Vec<String> = bound
                    .iter()
                    .chain(subject_drops)
                    .chain(pending_drops)
                    .cloned()
                    .collect();
                match tail {
                    Tail::Return => {
                        guard_and_body.push_str(&self.final_statement(
                            &clause.then,
                            Tail::Return,
                            &inner_indent,
                            &exit_drops,
                        ));
                    }
                    Tail::No => {
                        let value = self.expression(&clause.then, &inner_indent);
                        let result = self.fresh_name("r");
                        guard_and_body
                            .push_str(&format!("{inner_indent}const {result} = {value};\n"));
                        for binding in bound.iter().chain(subject_drops) {
                            guard_and_body
                                .push_str(&format!("{inner_indent}P.drop({binding});\n"));
                        }
                        guard_and_body.push_str(&format!(
                            "{inner_indent}break :{value_label} {result};\n"
                        ));
                    }
                }
                assert!(
                    self.reuse_token.is_none(),
                    "zig codegen: reuse token was not consumed by the clause body"
                );

                let breaks_label = format!("break :{clause_label}");
                let labelled = clause_body.contains(&breaks_label)
                    || guard_and_body.contains(&breaks_label);
                if labelled {
                    out.push_str(&format!("{indent}{clause_label}: {{\n"));
                } else {
                    out.push_str(&format!("{indent}{{\n"));
                }
                out.push_str(&clause_body);
                out.push_str(&binding_text);
                out.push_str(&guard_and_body);
                out.push_str(&format!("{indent}}}\n"));

                self.scope = saved_scope;
            }
        }
        out
    }

    fn pattern(&mut self, pattern: &TypedPattern, subject: &str) -> CompiledPattern {
        let mut compiled = CompiledPattern::default();
        self.compile_pattern(pattern, subject, &mut compiled);
        compiled
    }

    fn compile_pattern(
        &mut self,
        pattern: &TypedPattern,
        subject: &str,
        compiled: &mut CompiledPattern,
    ) {
        match pattern {
            Pattern::Discard { .. } => {}

            Pattern::Variable { name, .. } => {
                compiled
                    .bindings
                    .push((name.clone(), subject.to_string(), false));
            }

            Pattern::Assign { name, pattern, .. } => {
                compiled
                    .bindings
                    .push((name.clone(), subject.to_string(), false));
                self.compile_pattern(pattern, subject, compiled);
            }

            Pattern::Int { int_value, .. } => {
                compiled
                    .conditions
                    .push(format!("({subject}).int == {int_value}"));
            }

            Pattern::Float { value, .. } => {
                compiled
                    .conditions
                    .push(format!("({subject}).float == {value}"));
            }

            Pattern::String { value, .. } => {
                // Borrow-compare against the static literal bytes directly;
                // no allocation on the match path.
                compiled.conditions.push(format!(
                    "P.stringLiteralEquals({subject}, \"{}\")",
                    zig_string_contents(value)
                ));
            }

            Pattern::StringPrefix {
                left_side_string,
                left_side_assignment,
                right_side_assignment,
                ..
            } => {
                let prefix = zig_string_contents(left_side_string);
                compiled
                    .conditions
                    .push(format!("P.stringStartsWith({subject}, \"{prefix}\")"));
                if let Some((name, _)) = left_side_assignment {
                    compiled.bindings.push((
                        name.clone(),
                        format!("P.copyString(\"{prefix}\")"),
                        true,
                    ));
                }
                if let AssignName::Variable(name) = right_side_assignment {
                    compiled.bindings.push((
                        name.clone(),
                        format!(
                            "P.stringDropPrefix({subject}, {})",
                            decoded_string_byte_length(left_side_string)
                        ),
                        true,
                    ));
                }
            }

            Pattern::Tuple { elements, .. } => {
                for (index, element) in elements.iter().enumerate() {
                    let path = format!("({subject}).tuple[{index}]");
                    self.compile_pattern(element, &path, compiled);
                }
            }

            Pattern::List { elements, tail, .. } => {
                let mut cell = format!("({subject}).list");
                for element in elements {
                    compiled.conditions.push(format!("{cell} != null"));
                    let head = format!("{cell}.?.head");
                    self.compile_pattern(element, &head, compiled);
                    cell = format!("{cell}.?.tail");
                }
                match tail {
                    None => compiled.conditions.push(format!("{cell} == null")),
                    Some(tail_pattern) => {
                        let rest = format!("P.listValue({cell})");
                        self.compile_pattern(&tail_pattern.pattern, &rest, compiled);
                    }
                }
            }

            Pattern::Constructor {
                name,
                arguments,
                type_,
                ..
            } => {
                if type_.is_bool() {
                    match name.as_str() {
                        "True" => compiled.conditions.push(format!("({subject}).bool")),
                        "False" => compiled.conditions.push(format!("!(({subject}).bool)")),
                        _ => panic!("zig codegen: unexpected bool constructor {name}"),
                    }
                    return;
                }
                if type_.is_nil() {
                    return;
                }
                compiled
                    .conditions
                    .push(format!("P.recordHasName({subject}, \"{name}\")"));
                for (index, argument) in arguments.iter().enumerate() {
                    let path = format!("({subject}).record.fields[{index}]");
                    self.compile_pattern(&argument.value, &path, compiled);
                }
            }

            Pattern::BitArray { segments, .. } => {
                self.compile_bit_array_pattern(segments, subject, compiled);
            }
            // Sizes are handled inside compile_bit_array_pattern.
            Pattern::BitArraySize { .. } => {
                panic!("zig codegen: bit array size outside a bit array pattern")
            }

            Pattern::Invalid { .. } => {
                panic!("zig codegen: invalid pattern reached codegen")
            }
        }
    }

    /// What a bit array segment holds, derived from its options.
    /// v1 supports byte-aligned segments only.
    fn segment_layout<T>(
        options: &[crate::ast::BitArrayOption<T>],
    ) -> (SegmentKind, Option<&T>, bool, bool) {
        use crate::ast::BitArrayOption as Opt;
        let mut kind = SegmentKind::Int;
        let mut size = None;
        let mut signed = false;
        let mut little = false;
        for option in options {
            match option {
                Opt::Int { .. } => kind = SegmentKind::Int,
                Opt::Float { .. } => kind = SegmentKind::Float,
                Opt::Bytes { .. } => kind = SegmentKind::Bytes,
                Opt::Bits { .. } => kind = SegmentKind::Bits,
                Opt::Utf8 { .. } => kind = SegmentKind::Utf8,
                Opt::Utf8Codepoint { .. } => kind = SegmentKind::Utf8Codepoint,
                Opt::Signed { .. } => signed = true,
                Opt::Unsigned { .. } => signed = false,
                Opt::Little { .. } => little = true,
                Opt::Big { .. } => little = false,
                // Native endianness would make output machine-dependent.
                Opt::Native { .. } => little = cfg!(target_endian = "little"),
                Opt::Size { value, .. } => size = Some(value.as_ref()),
                Opt::Unit { value, .. } => {
                    if *value != 1 && *value != 8 {
                        panic!("zig codegen: bit array unit {value} is not supported yet")
                    }
                }
                Opt::Utf16 { .. }
                | Opt::Utf32 { .. }
                | Opt::Utf16Codepoint { .. }
                | Opt::Utf32Codepoint { .. } => {
                    panic!("zig codegen: utf16/utf32 bit array segments are not supported yet")
                }
            }
        }
        (kind, size, signed, little)
    }

    fn bit_array_construction(
        &mut self,
        segments: &[crate::ast::TypedExprBitArraySegment],
        indent: &str,
    ) -> String {
        let label = self.next_label("ba");
        let builder = self.fresh_name("b");
        let inner_indent = format!("{indent}{INDENT}");
        let mut out = format!("{label}: {{\n{inner_indent}var {builder} = P.baBuilder();\n");
        for segment in segments {
            let (kind, size, _signed, little) = Self::segment_layout(&segment.options);
            let value = self.expression(&segment.value, &inner_indent);
            match kind {
                SegmentKind::Int => {
                    let bits = size
                        .map(|size| self.size_bits_expression(size, &inner_indent))
                        .unwrap_or_else(|| "8".to_string());
                    out.push_str(&format!(
                        "{inner_indent}P.baAddInt(&{builder}, {value}, {bits}, {little});\n"
                    ));
                }
                SegmentKind::Float => {
                    let bits = size
                        .map(|size| self.size_bits_expression(size, &inner_indent))
                        .unwrap_or_else(|| "64".to_string());
                    out.push_str(&format!(
                        "{inner_indent}P.baAddFloat(&{builder}, {value}, {bits}, {little});\n"
                    ));
                }
                SegmentKind::Utf8 => {
                    out.push_str(&format!("{inner_indent}P.baAddUtf8(&{builder}, {value});\n"));
                }
                SegmentKind::Utf8Codepoint => {
                    out.push_str(&format!(
                        "{inner_indent}P.baAddUtf8Codepoint(&{builder}, {value});\n"
                    ));
                }
                SegmentKind::Bytes | SegmentKind::Bits => {
                    out.push_str(&format!("{inner_indent}P.baAddBits(&{builder}, {value});\n"));
                }
            }
        }
        out.push_str(&format!(
            "{inner_indent}break :{label} P.baFinish(&{builder});\n{indent}}}"
        ));
        out
    }

    /// A construction-side size expression, in bits. Literals are checked
    /// for byte alignment at compile time; runtime values at runtime.
    fn size_bits_expression(&mut self, size: &TypedExpr, indent: &str) -> String {
        match size {
            TypedExpr::Int { int_value, .. } => {
                let bits: usize = int_value
                    .clone()
                    .try_into()
                    .expect("zig codegen: negative bit array size");
                if bits % 8 != 0 {
                    panic!("zig codegen: non-byte-aligned bit array segments are not supported yet")
                }
                format!("{bits}")
            }
            _ => format!("P.baBitCount({})", self.expression(size, indent)),
        }
    }

    fn compile_bit_array_pattern(
        &mut self,
        segments: &[crate::ast::TypedPatternBitArraySegment],
        subject: &str,
        compiled: &mut CompiledPattern,
    ) {
        use crate::ast::BitArraySize;
        let matcher = self.fresh_name("m");
        compiled
            .setup
            .push(format!("var {matcher} = P.baMatcher({subject});"));

        // Slot indices for values extracted so far, by pattern-local name,
        // so later segment sizes can reference earlier int bindings.
        let mut int_slots: Vec<(EcoString, usize)> = Vec::new();
        let mut slot = 0;
        let mut ends_with_rest = false;

        // The runtime matcher has 16 fixed slots.
        if segments.len() > 16 {
            panic!(
                "zig codegen: bit array patterns with more than 16 segments are not supported yet"
            )
        }

        for (index, segment) in segments.iter().enumerate() {
            let is_last = index == segments.len() - 1;
            let (kind, size, signed, little) = Self::segment_layout(&segment.options);

            // A size expression: bits for ints/floats, bytes for byte
            // segments. Constant or a reference to an earlier int binding.
            // In patterns the size option wraps a Pattern; unwrap the
            // literal-or-variable forms.
            let size_expr = size.map(|size| {
                let inner: &BitArraySize<_> = match size {
                    Pattern::BitArraySize(inner) => inner,
                    Pattern::Int { int_value, .. } => {
                        let n: i64 = int_value
                            .try_into()
                            .expect("zig codegen: bit array size out of range");
                        return format!("{n}");
                    }
                    other => panic!(
                        "zig codegen: bit array size pattern is not supported yet: {other:?}"
                    ),
                };
                match inner {
                    BitArraySize::Int { int_value, .. } => {
                        let n: i64 = int_value
                            .try_into()
                            .expect("zig codegen: bit array size out of range");
                        format!("{n}")
                    }
                    BitArraySize::Variable { name, .. } => {
                        // Raw i64; the runtime extractors reject negative or
                        // misaligned sizes by failing the match.
                        if let Some((_, slot)) =
                            int_slots.iter().find(|(bound, _)| bound == name)
                        {
                            format!("{matcher}.ints[{slot}]")
                        } else {
                            let rendered = self.scope.get(name).cloned().unwrap_or_else(|| {
                                panic!(
                                    "zig codegen: bit array size variable {name} not in scope"
                                )
                            });
                            if self.raw_bindings.contains_key(&rendered) {
                                rendered
                            } else {
                                format!("({rendered}).int")
                            }
                        }
                    }
                    _ => panic!(
                        "zig codegen: bit array size expressions are not supported yet"
                    ),
                }
            });

            match kind {
                SegmentKind::Int => {
                    let bits = size_expr.unwrap_or_else(|| "8".to_string());
                    compiled.conditions.push(format!(
                        "P.baReadInt(&{matcher}, {bits}, {signed}, {little}, {slot})"
                    ));
                    match segment.value.as_ref() {
                        Pattern::Variable { name, .. } => {
                            int_slots.push((name.clone(), slot));
                            compiled.bindings.push((
                                name.clone(),
                                format!("P.baIntSlot(&{matcher}, {slot})"),
                                true,
                            ));
                        }
                        Pattern::Discard { .. } => {}
                        Pattern::Int { int_value, .. } => {
                            compiled
                                .conditions
                                .push(format!("{matcher}.ints[{slot}] == {int_value}"));
                        }
                        other => panic!(
                            "zig codegen: bit array int sub-pattern is not supported yet: {other:?}"
                        ),
                    }
                    slot += 1;
                }
                SegmentKind::Float => {
                    let bits = size_expr.unwrap_or_else(|| "64".to_string());
                    compiled.conditions.push(format!(
                        "P.baReadFloat(&{matcher}, {bits}, {little}, {slot})"
                    ));
                    match segment.value.as_ref() {
                        Pattern::Variable { name, .. } => {
                            compiled.bindings.push((
                                name.clone(),
                                format!("P.baFloatSlot(&{matcher}, {slot})"),
                                true,
                            ));
                        }
                        Pattern::Discard { .. } => {}
                        other => panic!(
                            "zig codegen: bit array float sub-pattern is not supported yet: {other:?}"
                        ),
                    }
                    slot += 1;
                }
                SegmentKind::Utf8Codepoint => {
                    compiled
                        .conditions
                        .push(format!("P.baReadUtf8Codepoint(&{matcher}, {slot})"));
                    match segment.value.as_ref() {
                        Pattern::Variable { name, .. } => {
                            int_slots.push((name.clone(), slot));
                            compiled.bindings.push((
                                name.clone(),
                                format!("P.baIntSlot(&{matcher}, {slot})"),
                                true,
                            ));
                        }
                        Pattern::Discard { .. } => {}
                        other => panic!(
                            "zig codegen: bit array codepoint sub-pattern is not supported yet: {other:?}"
                        ),
                    }
                    slot += 1;
                }
                SegmentKind::Utf8 => match segment.value.as_ref() {
                    Pattern::String { value, .. } => {
                        compiled.conditions.push(format!(
                            "P.baMatchLiteral(&{matcher}, \"{}\")",
                            zig_string_contents(value)
                        ));
                    }
                    other => panic!(
                        "zig codegen: utf8 bit array patterns only support string literals: {other:?}"
                    ),
                },
                SegmentKind::Bytes | SegmentKind::Bits => {
                    match size_expr {
                        Some(size) => {
                            let byte_count = match kind {
                                SegmentKind::Bits => format!("P.baBitsToBytes({size})"),
                                _ => size,
                            };
                            compiled.conditions.push(format!(
                                "P.baReadBytes(&{matcher}, {byte_count}, {slot})"
                            ));
                        }
                        None => {
                            if !is_last {
                                panic!(
                                    "zig codegen: unsized bytes segment must be last in a bit array pattern"
                                )
                            }
                            compiled
                                .conditions
                                .push(format!("P.baReadRest(&{matcher}, {slot})"));
                            ends_with_rest = true;
                        }
                    }
                    match segment.value.as_ref() {
                        Pattern::Variable { name, .. } => {
                            compiled.bindings.push((
                                name.clone(),
                                format!("P.baSliceSlot(&{matcher}, {subject}, {slot})"),
                                true,
                            ));
                        }
                        Pattern::Discard { .. } => {}
                        other => panic!(
                            "zig codegen: bytes sub-pattern is not supported yet: {other:?}"
                        ),
                    }
                    slot += 1;
                }
            }
        }
        if !ends_with_rest {
            compiled
                .conditions
                .push(format!("P.baAtEnd(&{matcher})"));
        }
    }

    fn guard(&mut self, guard: &TypedClauseGuard) -> String {
        match guard {
            crate::ast::ClauseGuard::Block { value, .. } => self.guard(value),
            crate::ast::ClauseGuard::Not { expression, .. } => {
                format!("P.negateBool({})", self.guard(expression))
            }
            crate::ast::ClauseGuard::BinaryOperator {
                operator,
                left,
                right,
                ..
            } => {
                let left = self.guard(left);
                let right = self.guard(right);
                binary_operator(*operator, &left, &right)
            }
            crate::ast::ClauseGuard::Var { name, .. } => {
                let rendered = self
                    .scope
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| panic!("zig codegen: guard variable {name} not in scope"));
                // A raw scalar binding boxes for the consuming helpers.
                if let Some(kind) = self.raw_bindings.get(&rendered) {
                    return format!("{}({rendered})", kind.box_helper());
                }
                // Guard operands feed consuming helpers (eq, tupleField...),
                // so each use takes its own reference like any other use.
                // Guards are branchy, so bindings here are never
                // move-approved and the dup is always correct.
                format!("P.dup({rendered})")
            }
            crate::ast::ClauseGuard::TupleIndex { tuple, index, .. } => {
                format!("P.tupleField({}, {index})", self.guard(tuple))
            }
            crate::ast::ClauseGuard::FieldAccess {
                container, index, ..
            } => {
                let index =
                    index.expect("zig codegen: guard field access without a resolved index");
                format!("P.recordField({}, {index})", self.guard(container))
            }
            crate::ast::ClauseGuard::Constant(constant) => self.constant(constant),
            crate::ast::ClauseGuard::ModuleSelect { .. } => {
                panic!("zig codegen: module access in guards is not supported yet")
            }
            crate::ast::ClauseGuard::Invalid { .. } => {
                panic!("zig codegen: invalid guard reached codegen")
            }
        }
    }

    fn constant(&mut self, constant: &Constant<std::sync::Arc<crate::type_::Type>>) -> String {
        match constant {
            Constant::Int { int_value, .. } => format!("P.intValue({int_value})"),
            Constant::Float { value, .. } => format!("P.floatValue({value})"),
            Constant::String { value, .. } => {
                format!("P.copyString(\"{}\")", zig_string_contents(value))
            }
            Constant::Tuple { elements, .. } => {
                let elements = elements
                    .iter()
                    .map(|element| self.constant(element))
                    .join(", ");
                format!("P.tupleValue(&[_]Value{{ {elements} }})")
            }
            Constant::List { elements, .. } => {
                if elements.is_empty() {
                    return "P.emptyList()".to_string();
                }
                let elements = elements
                    .iter()
                    .map(|element| self.constant(element))
                    .join(", ");
                format!("P.listFromSlice(&[_]Value{{ {elements} }}, P.emptyList())")
            }
            Constant::Record {
                name,
                arguments,
                record_constructor,
                ..
            } => {
                let (module, field_map) = record_constructor
                    .as_ref()
                    .and_then(|constructor| match &constructor.variant {
                        ValueConstructorVariant::Record {
                            module, field_map, ..
                        } => Some((module.clone(), field_map.clone())),
                        _ => None,
                    })
                    .unwrap_or_else(|| (PRELUDE_MODULE_NAME.into(), None));
                let arguments = arguments
                    .as_ref()
                    .map(|arguments| {
                        arguments
                            .iter()
                            .map(|argument| self.constant(&argument.value))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                self.record_construction(&module, name, &arguments, field_map.as_ref())
            }
            Constant::Var {
                name, constructor, ..
            } => {
                let constructor = constructor
                    .as_ref()
                    .expect("zig codegen: constant var with no constructor");
                self.variable(name, &constructor.variant)
            }
            Constant::BinaryOperator {
                operator,
                left,
                right,
                ..
            } => {
                let left = self.constant(left);
                let right = self.constant(right);
                binary_operator(*operator, &left, &right)
            }
            _ => {
                panic!("zig codegen: constant is not supported yet: {constant:?}")
            }
        }
    }

    fn panic_message(
        &mut self,
        message: Option<&TypedExpr>,
        default: &str,
        indent: &str,
    ) -> String {
        // Messages evaluate only on the failure path.
        let saved_barrier = self.reuse_barrier;
        self.reuse_barrier += 1;
        let result = match message {
            Some(message) => format!("({}).string", self.expression(message, indent)),
            None => format!("\"{default}\""),
        };
        self.reuse_barrier = saved_barrier;
        result
    }

    fn variable(&mut self, name: &EcoString, variant: &ValueConstructorVariant) -> String {
        match variant {
            ValueConstructorVariant::LocalVariable { .. } => {
                let rendered = self
                    .scope
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| panic!("zig codegen: variable {name} not in scope"));
                // A raw scalar binding boxes at this polymorphic use.
                if let Some(kind) = self.raw_bindings.get(&rendered) {
                    return format!("{}({rendered})", kind.box_helper());
                }
                // A move-approved binding transfers its reference at this
                // (single) use; otherwise the use takes its own reference
                // and the binding's is released at scope exit.
                if self.moved.remove(&rendered) {
                    rendered
                } else {
                    format!("P.dup({rendered})")
                }
            }
            ValueConstructorVariant::Record {
                name,
                module,
                arity,
                field_map,
                ..
            } => {
                if *arity == 0 {
                    self.record_construction(&module.clone(), &name.clone(), &[], None)
                } else {
                    self.constructor_reference(
                        &module.clone(),
                        &name.clone(),
                        *arity as usize,
                        field_map.clone().as_ref(),
                    )
                }
            }
            ValueConstructorVariant::ModuleFn {
                name,
                module,
                arity,
                ..
            } => self.function_reference(&module.clone(), &name.clone(), *arity),
            ValueConstructorVariant::ModuleConstant { module, name, .. } => {
                if *module == self.module.module_name {
                    format!("{}()", constant_identifier(name))
                } else {
                    let _ = self.module.modules_used.insert(module.clone());
                    format!("{}.{}()", module_ref(module), constant_identifier(name))
                }
            }
        }
    }

    fn line_number(&self, location: &SrcSpan) -> u32 {
        self.module.line_numbers.line_number(location.start)
    }

    fn next_label(&mut self, prefix: &str) -> String {
        let label = format!("{prefix}{}", self.label_counter);
        self.label_counter += 1;
        label
    }

    fn fresh_name(&mut self, prefix: &str) -> String {
        let mut counter = 0;
        loop {
            let candidate = EcoString::from(format!("{prefix}${counter}"));
            if !self.used_names.contains(&candidate) {
                let _ = self.used_names.insert(candidate.clone());
                return zig_identifier(&candidate);
            }
            counter += 1;
        }
    }

    /// Bind a source-level name, renaming to keep rendered names unique
    /// within the function so shadowing works and usage scans are exact.
    fn bind(&mut self, name: &EcoString) -> String {
        // The v$ prefix keeps locals out of the module-level namespace:
        // zig errors when a local shadows any file-scope declaration.
        let mut rendered: EcoString = EcoString::from(format!("v${name}"));
        let mut counter = 0;
        while self.used_names.contains(&rendered) {
            counter += 1;
            rendered = EcoString::from(format!("v${name}${counter}"));
        }
        let _ = self.used_names.insert(rendered.clone());
        let identifier = zig_identifier(&rendered);
        let _ = self.scope.insert(name.clone(), identifier.clone());
        identifier
    }
}

/// True when a clause is the canonical list-reuse shape: it matches
/// `[x, ..rest]` on a single subject and its body's always-evaluated part
/// is (or directly contains, as a call argument) a `[y, ..zs]`
/// construction. The construction is then guaranteed to render exactly
/// once, so the matched cell can be handed to it for in-place reuse.
fn clause_reuse_kind(clause: &TypedClause) -> Option<ReuseKind> {
    if !clause.alternative_patterns.is_empty() || clause.pattern.len() != 1 {
        return None;
    }
    // The allocation shape the clause's pattern matched.
    let matched = match &clause.pattern[0] {
        Pattern::List { elements, tail, .. } if elements.len() == 1 && tail.is_some() => {
            ReuseKind::Cons
        }
        Pattern::Constructor {
            arguments, type_, ..
        } if !type_.is_bool() && !type_.is_nil() && !arguments.is_empty() => {
            ReuseKind::Record(arguments.len())
        }
        Pattern::Tuple { elements, .. } if !elements.is_empty() => {
            ReuseKind::Tuple(elements.len())
        }
        _ => return None,
    };
    // A construction the body is guaranteed to render, with the same
    // allocation shape as the match.
    fn is_matching_construction(expression: &TypedExpr, matched: ReuseKind) -> bool {
        match (expression, matched) {
            (TypedExpr::List { elements, tail, .. }, ReuseKind::Cons) => {
                elements.len() == 1 && tail.is_some()
            }
            (TypedExpr::Tuple { elements, .. }, ReuseKind::Tuple(arity)) => {
                elements.len() == arity
            }
            (TypedExpr::Call { fun, arguments, .. }, ReuseKind::Record(arity)) => {
                arguments.len() == arity
                    && match fun.as_ref() {
                        TypedExpr::Var { constructor, .. } => matches!(
                            &constructor.variant,
                            ValueConstructorVariant::Record { .. }
                        ),
                        TypedExpr::ModuleSelect { constructor, .. } => {
                            matches!(constructor, ModuleValueConstructor::Record { .. })
                        }
                        _ => false,
                    }
            }
            _ => false,
        }
    }
    match &clause.then {
        expression if is_matching_construction(expression, matched) => Some(matched),
        // Call arguments are always evaluated, so a construction there is
        // guaranteed to render (covers accumulator-style tail calls).
        TypedExpr::Call { arguments, .. } => arguments
            .iter()
            .any(|argument| is_matching_construction(&argument.value, matched))
            .then_some(matched),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum SegmentKind {
    Int,
    Float,
    /// Size option counts bytes (`bytes-size(n)`).
    Bytes,
    /// Size option counts bits (`bits-size(n)`).
    Bits,
    Utf8,
    Utf8Codepoint,
}

#[derive(Default)]
struct CompiledPattern {
    /// Statements emitted before the condition test (bit array matchers).
    setup: Vec<String>,
    conditions: Vec<String>,
    /// (source name, zig expression, owned). Non-owned expressions borrow
    /// into the subject and are wrapped in P.dup by the emitter.
    bindings: Vec<(EcoString, String, bool)>,
}

const BREAK_PLACEHOLDER: &str = "\u{1}break\u{1}";

fn binary_operator(operator: BinOp, left: &str, right: &str) -> String {
    let helper = match operator {
        BinOp::And => {
            return format!("P.boolValue(if (({left}).bool) ({right}).bool else false)");
        }
        BinOp::Or => {
            return format!("P.boolValue(if (({left}).bool) true else ({right}).bool)");
        }
        BinOp::Eq => "eq",
        BinOp::NotEq => "notEq",
        BinOp::LtInt => "ltInt",
        BinOp::LtEqInt => "ltEqInt",
        BinOp::GtInt => "gtInt",
        BinOp::GtEqInt => "gtEqInt",
        BinOp::LtFloat => "ltFloat",
        BinOp::LtEqFloat => "ltEqFloat",
        BinOp::GtFloat => "gtFloat",
        BinOp::GtEqFloat => "gtEqFloat",
        BinOp::AddInt => "addInt",
        BinOp::SubInt => "subInt",
        BinOp::MultInt => "multInt",
        BinOp::DivInt => "divInt",
        BinOp::RemainderInt => "remainderInt",
        BinOp::AddFloat => "addFloat",
        BinOp::SubFloat => "subFloat",
        BinOp::MultFloat => "multFloat",
        BinOp::DivFloat => "divFloat",
        BinOp::Concatenate => "concatenate",
    };
    format!("P.{helper}({left}, {right})")
}

/// True if the function's body contains a tail call to `name`, in which case
/// the function is generated as a loop.
fn body_has_tail_self_call(body: &[TypedStatement], name: &EcoString) -> bool {
    let Some(last) = body.last() else {
        return false;
    };
    match last {
        Statement::Expression(expression) => expression_has_tail_self_call(expression, name),
        Statement::Use(use_) => expression_has_tail_self_call(&use_.call, name),
        Statement::Assignment(_) | Statement::Assert(_) => false,
    }
}

fn expression_has_tail_self_call(expression: &TypedExpr, name: &EcoString) -> bool {
    match expression {
        TypedExpr::Call { fun, .. } => match fun.as_ref() {
            TypedExpr::Var { constructor, .. } => matches!(
                &constructor.variant,
                ValueConstructorVariant::ModuleFn { name: fn_name, .. } if fn_name == name
            ),
            _ => false,
        },
        TypedExpr::Case { clauses, .. } => clauses
            .iter()
            .any(|clause| expression_has_tail_self_call(&clause.then, name)),
        TypedExpr::Block { statements, .. } => {
            body_has_tail_self_call(statements.as_slice(), name)
        }
        TypedExpr::Pipeline { finally, .. } => expression_has_tail_self_call(finally, name),
        _ => false,
    }
}

/// True when every occurrence of `name` in the body is the container of a
/// field access — the shape a borrowed reference can serve. Conservative:
/// any bare occurrence, any rebind of the same source name (shadowing is
/// not tracked), any lambda mentioning it (the closure could outlive the
/// call), and any guard use (guards feed consuming helpers) disqualify.
fn param_uses_are_borrow_only(name: &EcoString, body: &[TypedStatement]) -> bool {
    body.iter().all(|statement| statement_borrow_only(name, statement))
}

fn statement_borrow_only(name: &EcoString, statement: &TypedStatement) -> bool {
    match statement {
        Statement::Expression(expression) => expression_borrow_only(name, expression),
        Statement::Assignment(assignment) => {
            !pattern_binds(name, &assignment.pattern)
                && expression_borrow_only(name, &assignment.value)
        }
        Statement::Use(use_) => expression_borrow_only(name, &use_.call),
        Statement::Assert(assert) => {
            expression_borrow_only(name, &assert.value)
                && assert
                    .message
                    .as_ref()
                    .is_none_or(|message| expression_borrow_only(name, message))
        }
    }
}

fn expression_borrow_only(name: &EcoString, expression: &TypedExpr) -> bool {
    match expression {
        TypedExpr::Var {
            constructor,
            name: used,
            ..
        } => {
            // A bare occurrence is a consuming use.
            !(used == name
                && matches!(
                    constructor.variant,
                    ValueConstructorVariant::LocalVariable { .. }
                ))
        }
        // The borrow shape: the name as a field-access container.
        TypedExpr::RecordAccess { record, .. }
        | TypedExpr::PositionalAccess { record, .. } => match record.as_ref() {
            TypedExpr::Var { name: used, .. } if used == name => true,
            other => expression_borrow_only(name, other),
        },
        TypedExpr::TupleIndex { tuple, .. } => match tuple.as_ref() {
            TypedExpr::Var { name: used, .. } if used == name => true,
            other => expression_borrow_only(name, other),
        },
        // A lambda mentioning the name would capture a reference that can
        // outlive the borrowed call.
        TypedExpr::Fn { body, .. } => {
            let mut names = BTreeSet::new();
            for statement in body {
                statement_variables(statement, &mut names);
            }
            !names.contains(name)
        }
        TypedExpr::Int { .. }
        | TypedExpr::Float { .. }
        | TypedExpr::String { .. }
        | TypedExpr::ModuleSelect { .. }
        | TypedExpr::Invalid { .. } => true,
        TypedExpr::Block { statements, .. } => statements
            .iter()
            .all(|statement| statement_borrow_only(name, statement)),
        TypedExpr::Pipeline {
            first_value,
            assignments,
            finally,
            ..
        } => {
            expression_borrow_only(name, &first_value.value)
                && assignments
                    .iter()
                    .all(|(assignment, _)| expression_borrow_only(name, &assignment.value))
                && expression_borrow_only(name, finally)
        }
        TypedExpr::List { elements, tail, .. } => {
            elements
                .iter()
                .all(|element| expression_borrow_only(name, element))
                && tail
                    .as_ref()
                    .is_none_or(|tail| expression_borrow_only(name, tail))
        }
        TypedExpr::Call { fun, arguments, .. } => {
            expression_borrow_only(name, fun)
                && arguments
                    .iter()
                    .all(|argument| expression_borrow_only(name, &argument.value))
        }
        TypedExpr::BinOp { left, right, .. } => {
            expression_borrow_only(name, left) && expression_borrow_only(name, right)
        }
        TypedExpr::Case {
            subjects, clauses, ..
        } => {
            subjects
                .iter()
                .all(|subject| expression_borrow_only(name, subject))
                && clauses.iter().all(|clause| {
                    let mut patterns = vec![&clause.pattern];
                    patterns.extend(clause.alternative_patterns.iter());
                    let rebinds = patterns.iter().any(|multi| {
                        multi.iter().any(|pattern| pattern_binds(name, pattern))
                    });
                    let guard_mentions = clause.guard.as_ref().is_some_and(|guard| {
                        let mut names = BTreeSet::new();
                        guard_variables(guard, &mut names);
                        names.contains(name)
                    });
                    !rebinds
                        && !guard_mentions
                        && expression_borrow_only(name, &clause.then)
                })
        }
        TypedExpr::Tuple { elements, .. } => elements
            .iter()
            .all(|element| expression_borrow_only(name, element)),
        TypedExpr::Todo { message, .. } | TypedExpr::Panic { message, .. } => message
            .as_ref()
            .is_none_or(|message| expression_borrow_only(name, message)),
        TypedExpr::Echo {
            expression: inner,
            message,
            ..
        } => {
            inner
                .as_ref()
                .is_none_or(|inner| expression_borrow_only(name, inner))
                && message
                    .as_ref()
                    .is_none_or(|message| expression_borrow_only(name, message))
        }
        TypedExpr::RecordUpdate {
            updated_record,
            constructor,
            arguments,
            ..
        } => {
            expression_borrow_only(name, updated_record)
                && expression_borrow_only(name, constructor)
                && arguments
                    .iter()
                    .all(|argument| expression_borrow_only(name, &argument.value))
        }
        TypedExpr::NegateBool { value, .. } | TypedExpr::NegateInt { value, .. } => {
            expression_borrow_only(name, value)
        }
        TypedExpr::BitArray { segments, .. } => segments.iter().all(|segment| {
            expression_borrow_only(name, &segment.value)
                && segment.options.iter().all(|option| {
                    option
                        .value()
                        .is_none_or(|size| expression_borrow_only(name, size))
                })
        }),
    }
}

/// How a name is used in a region of code, for the conservative last-use
/// move optimisation: a binding used exactly once, not under any branching
/// construct (case clause, guard) or lambda, can transfer its reference at
/// that use instead of dup-at-use + drop-at-scope-exit.
#[derive(Default, Clone, Copy)]
struct UseSummary {
    count: usize,
    under_branch_or_lambda: bool,
}

impl UseSummary {
    fn single_straight_line_use(&self) -> bool {
        self.count == 1 && !self.under_branch_or_lambda
    }
}

/// Count uses of `name` in the statements following its binding. A rebind
/// of the same name in the sequence ends the scan (later occurrences belong
/// to the new binding). Rebinds in nested scopes are not tracked and only
/// inflate the count, which fails safe (the optimisation is skipped).
fn summarise_uses(name: &EcoString, statements: &[TypedStatement]) -> UseSummary {
    let mut summary = UseSummary::default();
    for statement in statements {
        match statement {
            Statement::Expression(expression) => {
                expression_uses(name, expression, false, &mut summary)
            }
            Statement::Assignment(assignment) => {
                expression_uses(name, &assignment.value, false, &mut summary);
                if pattern_binds(name, &assignment.pattern) {
                    break;
                }
            }
            Statement::Use(use_) => expression_uses(name, &use_.call, false, &mut summary),
            Statement::Assert(assert) => {
                expression_uses(name, &assert.value, false, &mut summary);
                if let Some(message) = &assert.message {
                    // Assert messages are evaluated only on the failure path.
                    expression_uses(name, message, true, &mut summary);
                }
            }
        }
    }
    summary
}

/// Uses inside nested statement sequences (blocks, lambda bodies). Rebinds
/// are not tracked here; inner shadowing only inflates counts (fails safe).
fn statement_uses(
    name: &EcoString,
    statement: &TypedStatement,
    branchy: bool,
    summary: &mut UseSummary,
) {
    match statement {
        Statement::Expression(expression) => expression_uses(name, expression, branchy, summary),
        Statement::Assignment(assignment) => {
            expression_uses(name, &assignment.value, branchy, summary)
        }
        Statement::Use(use_) => expression_uses(name, &use_.call, branchy, summary),
        Statement::Assert(assert) => {
            expression_uses(name, &assert.value, branchy, summary);
            if let Some(message) = &assert.message {
                expression_uses(name, message, true, summary);
            }
        }
    }
}

fn pattern_binds(name: &EcoString, pattern: &TypedPattern) -> bool {
    let mut compiled = CompiledPattern::default();
    // Reuse the binding collector: cheap and complete. Conditions produced
    // here are discarded.
    collect_pattern_names(pattern, &mut compiled);
    compiled.bindings.iter().any(|(bound, _, _)| bound == name)
}

fn collect_pattern_names(pattern: &TypedPattern, compiled: &mut CompiledPattern) {
    match pattern {
        Pattern::Variable { name, .. } => {
            compiled.bindings.push((name.clone(), String::new(), false));
        }
        Pattern::Assign { name, pattern, .. } => {
            compiled.bindings.push((name.clone(), String::new(), false));
            collect_pattern_names(pattern, compiled);
        }
        Pattern::StringPrefix {
            left_side_assignment,
            right_side_assignment,
            ..
        } => {
            if let Some((name, _)) = left_side_assignment {
                compiled.bindings.push((name.clone(), String::new(), false));
            }
            if let AssignName::Variable(name) = right_side_assignment {
                compiled.bindings.push((name.clone(), String::new(), false));
            }
        }
        Pattern::Tuple { elements, .. } => {
            for element in elements {
                collect_pattern_names(element, compiled);
            }
        }
        Pattern::List { elements, tail, .. } => {
            for element in elements {
                collect_pattern_names(element, compiled);
            }
            if let Some(tail) = tail {
                collect_pattern_names(&tail.pattern, compiled);
            }
        }
        Pattern::Constructor { arguments, .. } => {
            for argument in arguments {
                collect_pattern_names(&argument.value, compiled);
            }
        }
        Pattern::BitArray { segments, .. } => {
            for segment in segments {
                collect_pattern_names(&segment.value, compiled);
            }
        }
        Pattern::Int { .. }
        | Pattern::Float { .. }
        | Pattern::String { .. }
        | Pattern::Discard { .. }
        | Pattern::BitArraySize { .. }
        | Pattern::Invalid { .. } => {}
    }
}

fn expression_uses(
    name: &EcoString,
    expression: &TypedExpr,
    branchy: bool,
    summary: &mut UseSummary,
) {
    match expression {
        TypedExpr::Var {
            constructor,
            name: used,
            ..
        } => {
            if used == name
                && matches!(
                    constructor.variant,
                    ValueConstructorVariant::LocalVariable { .. }
                )
            {
                summary.count += 1;
                if branchy {
                    summary.under_branch_or_lambda = true;
                }
            }
        }
        TypedExpr::Int { .. }
        | TypedExpr::Float { .. }
        | TypedExpr::String { .. }
        | TypedExpr::ModuleSelect { .. }
        | TypedExpr::Invalid { .. } => {}
        TypedExpr::BitArray { segments, .. } => {
            for segment in segments {
                expression_uses(name, &segment.value, branchy, summary);
                for option in &segment.options {
                    if let Some(size) = option.value() {
                        expression_uses(name, size, branchy, summary);
                    }
                }
            }
        }
        TypedExpr::Block { statements, .. } => {
            for statement in statements {
                statement_uses(name, statement, branchy, summary);
            }
        }
        TypedExpr::Pipeline {
            first_value,
            assignments,
            finally,
            ..
        } => {
            expression_uses(name, &first_value.value, branchy, summary);
            for (assignment, _) in assignments {
                expression_uses(name, &assignment.value, branchy, summary);
            }
            expression_uses(name, finally, branchy, summary);
        }
        // Anything inside a lambda runs zero or many times; never a
        // straight-line use.
        TypedExpr::Fn { body, .. } => {
            for statement in body {
                statement_uses(name, statement, true, summary);
            }
        }
        TypedExpr::List { elements, tail, .. } => {
            for element in elements {
                expression_uses(name, element, branchy, summary);
            }
            if let Some(tail) = tail {
                expression_uses(name, tail, branchy, summary);
            }
        }
        TypedExpr::Call { fun, arguments, .. } => {
            expression_uses(name, fun, branchy, summary);
            for argument in arguments {
                expression_uses(name, &argument.value, branchy, summary);
            }
        }
        TypedExpr::BinOp {
            operator,
            left,
            right,
            ..
        } => {
            expression_uses(name, left, branchy, summary);
            // The right operand of a short-circuit operator may not run.
            let conditional = matches!(operator, BinOp::And | BinOp::Or);
            expression_uses(name, right, branchy || conditional, summary);
        }
        TypedExpr::Case {
            subjects, clauses, ..
        } => {
            for subject in subjects {
                expression_uses(name, subject, branchy, summary);
            }
            for clause in clauses {
                // Clause bodies and guards are conditional.
                expression_uses(name, &clause.then, true, summary);
                if let Some(guard) = &clause.guard {
                    guard_uses(name, guard, summary);
                }
                // Clause-pattern rebinds are not tracked; their inner
                // uses inflate the count, which fails safe.
            }
        }
        TypedExpr::RecordAccess { record, .. } => {
            expression_uses(name, record, branchy, summary)
        }
        TypedExpr::PositionalAccess { record, .. } => {
            expression_uses(name, record, branchy, summary)
        }
        TypedExpr::Tuple { elements, .. } => {
            for element in elements {
                expression_uses(name, element, branchy, summary);
            }
        }
        TypedExpr::TupleIndex { tuple, .. } => expression_uses(name, tuple, branchy, summary),
        TypedExpr::Todo { message, .. } | TypedExpr::Panic { message, .. } => {
            if let Some(message) = message {
                expression_uses(name, message, true, summary);
            }
        }
        TypedExpr::Echo {
            expression: inner,
            message,
            ..
        } => {
            if let Some(inner) = inner {
                expression_uses(name, inner, branchy, summary);
            }
            if let Some(message) = message {
                expression_uses(name, message, branchy, summary);
            }
        }
        TypedExpr::RecordUpdate {
            updated_record,
            constructor,
            arguments,
            ..
        } => {
            // Treated as branchy: with every field explicit the generated
            // code never renders this reference, so a move here would leak.
            expression_uses(name, updated_record, true, summary);
            expression_uses(name, constructor, branchy, summary);
            for argument in arguments {
                expression_uses(name, &argument.value, branchy, summary);
            }
        }
        TypedExpr::NegateBool { value, .. } | TypedExpr::NegateInt { value, .. } => {
            expression_uses(name, value, branchy, summary);
        }
    }
}

fn guard_uses(name: &EcoString, guard: &TypedClauseGuard, summary: &mut UseSummary) {
    match guard {
        crate::ast::ClauseGuard::Var { name: used, .. } => {
            if used == name {
                summary.count += 1;
                summary.under_branch_or_lambda = true;
            }
        }
        crate::ast::ClauseGuard::Block { value, .. } => guard_uses(name, value, summary),
        crate::ast::ClauseGuard::Not { expression, .. } => guard_uses(name, expression, summary),
        crate::ast::ClauseGuard::BinaryOperator { left, right, .. } => {
            guard_uses(name, left, summary);
            guard_uses(name, right, summary);
        }
        crate::ast::ClauseGuard::TupleIndex { tuple, .. } => guard_uses(name, tuple, summary),
        crate::ast::ClauseGuard::FieldAccess { container, .. } => {
            guard_uses(name, container, summary)
        }
        crate::ast::ClauseGuard::ModuleSelect { .. }
        | crate::ast::ClauseGuard::Constant(_)
        | crate::ast::ClauseGuard::Invalid { .. } => {}
    }
}

/// Names of variables an anonymous function's body references. This
/// overapproximates (a name bound inside the body shadows captures but is
/// still collected); harmless, as capturing an extra value is sound.
fn free_variables(body: &[TypedStatement]) -> BTreeSet<EcoString> {
    let mut names = BTreeSet::new();
    for statement in body {
        statement_variables(statement, &mut names);
    }
    names
}

fn statement_variables(statement: &TypedStatement, names: &mut BTreeSet<EcoString>) {
    match statement {
        Statement::Expression(expression) => expression_variables(expression, names),
        Statement::Assignment(assignment) => expression_variables(&assignment.value, names),
        Statement::Use(use_) => expression_variables(&use_.call, names),
        Statement::Assert(assert) => {
            expression_variables(&assert.value, names);
            if let Some(message) = &assert.message {
                expression_variables(message, names);
            }
        }
    }
}

fn expression_variables(expression: &TypedExpr, names: &mut BTreeSet<EcoString>) {
    match expression {
        TypedExpr::Var {
            constructor, name, ..
        } => {
            if matches!(
                constructor.variant,
                ValueConstructorVariant::LocalVariable { .. }
            ) {
                let _ = names.insert(name.clone());
            }
        }
        TypedExpr::Int { .. }
        | TypedExpr::Float { .. }
        | TypedExpr::String { .. }
        | TypedExpr::ModuleSelect { .. }
        | TypedExpr::Invalid { .. } => {}
        TypedExpr::Block { statements, .. } => {
            for statement in statements {
                statement_variables(statement, names);
            }
        }
        TypedExpr::Pipeline {
            first_value,
            assignments,
            finally,
            ..
        } => {
            expression_variables(&first_value.value, names);
            for (assignment, _) in assignments {
                expression_variables(&assignment.value, names);
            }
            expression_variables(finally, names);
        }
        TypedExpr::Fn { body, .. } => {
            for statement in body {
                statement_variables(statement, names);
            }
        }
        TypedExpr::List { elements, tail, .. } => {
            for element in elements {
                expression_variables(element, names);
            }
            if let Some(tail) = tail {
                expression_variables(tail, names);
            }
        }
        TypedExpr::Call { fun, arguments, .. } => {
            expression_variables(fun, names);
            for argument in arguments {
                expression_variables(&argument.value, names);
            }
        }
        TypedExpr::BinOp { left, right, .. } => {
            expression_variables(left, names);
            expression_variables(right, names);
        }
        TypedExpr::Case {
            subjects, clauses, ..
        } => {
            for subject in subjects {
                expression_variables(subject, names);
            }
            for clause in clauses {
                expression_variables(&clause.then, names);
                if let Some(guard) = &clause.guard {
                    guard_variables(guard, names);
                }
            }
        }
        TypedExpr::RecordAccess { record, .. } => expression_variables(record, names),
        TypedExpr::PositionalAccess { record, .. } => expression_variables(record, names),
        TypedExpr::Tuple { elements, .. } => {
            for element in elements {
                expression_variables(element, names);
            }
        }
        TypedExpr::TupleIndex { tuple, .. } => expression_variables(tuple, names),
        TypedExpr::Todo { message, .. } | TypedExpr::Panic { message, .. } => {
            if let Some(message) = message {
                expression_variables(message, names);
            }
        }
        TypedExpr::Echo {
            expression,
            message,
            ..
        } => {
            if let Some(expression) = expression {
                expression_variables(expression, names);
            }
            if let Some(message) = message {
                expression_variables(message, names);
            }
        }
        TypedExpr::BitArray { segments, .. } => {
            for segment in segments {
                expression_variables(&segment.value, names);
            }
        }
        TypedExpr::RecordUpdate {
            updated_record,
            constructor,
            arguments,
            ..
        } => {
            expression_variables(updated_record, names);
            expression_variables(constructor, names);
            for argument in arguments {
                expression_variables(&argument.value, names);
            }
        }
        TypedExpr::NegateBool { value, .. } | TypedExpr::NegateInt { value, .. } => {
            expression_variables(value, names);
        }
    }
}

fn guard_variables(guard: &TypedClauseGuard, names: &mut BTreeSet<EcoString>) {
    match guard {
        crate::ast::ClauseGuard::Var { name, .. } => {
            let _ = names.insert(name.clone());
        }
        crate::ast::ClauseGuard::Block { value, .. } => guard_variables(value, names),
        crate::ast::ClauseGuard::Not { expression, .. } => guard_variables(expression, names),
        crate::ast::ClauseGuard::BinaryOperator { left, right, .. } => {
            guard_variables(left, names);
            guard_variables(right, names);
        }
        crate::ast::ClauseGuard::TupleIndex { tuple, .. } => guard_variables(tuple, names),
        crate::ast::ClauseGuard::FieldAccess { container, .. } => {
            guard_variables(container, names)
        }
        crate::ast::ClauseGuard::ModuleSelect { .. }
        | crate::ast::ClauseGuard::Constant(_)
        | crate::ast::ClauseGuard::Invalid { .. } => {}
    }
}

fn function_arity(expression: &TypedExpr) -> usize {
    expression
        .type_()
        .fn_arity()
        .expect("zig codegen: function reference with non-function type")
}

/// Quote an identifier with zig's raw identifier syntax. This side-steps
/// zig keyword collisions and allows `$` in generated names.
fn zig_identifier(name: &str) -> String {
    format!("@\"{name}\"")
}

/// Gleam string literal contents are close enough to zig's escape syntax to
/// pass through, except form feed which zig does not support, and literal
/// control characters (multi-line strings) which zig literals cannot hold.
fn zig_string_contents(value: &str) -> String {
    value
        .replace("\\f", "\\x0C")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

/// Byte length of a Gleam string literal's decoded value, for string prefix
/// pattern slicing.
fn decoded_string_byte_length(value: &str) -> usize {
    let mut length = 0;
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\\' {
            length += character.len_utf8();
            continue;
        }
        match characters.next() {
            Some('u') => {
                // \u{XXXX}
                let mut digits = String::new();
                if characters.peek() == Some(&'{') {
                    let _ = characters.next();
                    while let Some(&digit) = characters.peek() {
                        let _ = characters.next();
                        if digit == '}' {
                            break;
                        }
                        digits.push(digit);
                    }
                }
                let code_point = u32::from_str_radix(&digits, 16).unwrap_or(0);
                length += char::from_u32(code_point).map(char::len_utf8).unwrap_or(1);
            }
            Some(_) => length += 1,
            None => {}
        }
    }
    length
}
