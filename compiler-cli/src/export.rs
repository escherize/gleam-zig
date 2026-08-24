// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2022 The Gleam contributors

use crate::fs::{self, ZipArchive};
use camino::Utf8PathBuf;
use gleam_core::{
    Result,
    analyse::TargetSupport,
    build::{Codegen, Compile, Mode, Options, Target},
    paths::ProjectPaths,
    type_::ModuleFunction,
};
use std::io::Cursor;

static ENTRYPOINT_FILENAME_POWERSHELL: &str = "entrypoint.ps1";
static ENTRYPOINT_FILENAME_POSIX_SHELL: &str = "entrypoint.sh";

static ENTRYPOINT_TEMPLATE_POWERSHELL: &str =
    include_str!("../templates/erlang-shipment-entrypoint.ps1");
static ENTRYPOINT_TEMPLATE_POSIX_SHELL: &str =
    include_str!("../templates/erlang-shipment-entrypoint.sh");

/// Generate a single file of precompiled Erlang, suitable for CLIs.
///
pub fn escript(paths: &ProjectPaths) -> Result<()> {
    let target = Target::Erlang;
    let mode = Mode::Prod;
    let build = paths.build_directory_for_target(mode, target);

    // Reset the directories to ensure we have a clean slate and no old code
    fs::delete_directory(&build)?;

    let manifest = crate::build::download_dependencies(paths, crate::cli::Reporter::new())?;

    // Build project in production mode
    let build_options = Options {
        root_target_support: TargetSupport::Enforced,
        warnings_as_errors: false,
        codegen: Codegen::All,
        compile: Compile::All,
        mode,
        target: Some(target),
        no_print_progress: false,
    };
    let built = crate::build::main(paths, build_options, manifest)?;
    let package_name = &built.root_package.config.name;

    // The main function must exist for the escript to call. This will return an
    // error if it could not be found.
    let _: ModuleFunction = built.get_main_function(package_name, target)?;

    // Create the zip archive for the code
    let mut zip = ZipArchive::new(Cursor::new(Vec::new()));

    for entry in fs::read_dir(&build)?.filter_map(Result::ok) {
        let ebin = entry.path().join("ebin");

        // We want the ebin code directories for each package
        if !ebin.is_dir() {
            continue;
        }

        for entry in fs::read_dir(&ebin)?.filter_map(Result::ok) {
            let path = entry.path();
            let extension = path.extension().unwrap_or_default();

            let Some(name) = path.file_name() else {
                continue;
            };

            if !path.is_file() {
                continue;
            }

            // We want to copy compiled BEAM bytecode and app configuration files
            if extension != "beam" && extension != "app" {
                continue;
            }

            zip.add_file_from_disc(path, name)?;
        }
    }

    let zip = zip.finish()?.into_inner();

    let escript_path = paths.root().join(package_name.as_str());
    let mut file = fs::open_file(&escript_path)?;

    // The -escript flag in the header instructs the BEAM `escript` program
    // to run the regular Gleam entrypoint module when running this escript.
    let header = format!(
        "#!/usr/bin/env escript
%%
%%!-escript main {package_name}@@main
"
    );

    fs::write_to_open_file(&mut file, &escript_path, header)?;
    fs::write_to_open_file(&mut file, &escript_path, zip)?;
    fs::make_executable(&escript_path)?;

    // Windows shells largely do not use shebangs, so for the escript to be
    // directly executable a .cmd wrapper script is provided.
    if cfg!(windows) {
        let cmd_path = escript_path.with_extension("cmd");
        fs::write(&cmd_path, "@echo off\r\nescript.exe \"%~dpn0\" %*\r\n")?;
    }

    println!(
        "
Your escript has been generated to {escript_path}.
",
    );

    Ok(())
}

/// Generate a directory of precompiled Erlang along with a start script.
/// Suitable for deployment to a server.
///
/// For each Erlang application (aka package) directory these directories are
/// copied across:
/// - ebin
/// - include
/// - priv
pub(crate) fn erlang_shipment(paths: &ProjectPaths) -> Result<()> {
    let target = Target::Erlang;
    let mode = Mode::Prod;
    let build = paths.build_directory_for_target(mode, target);
    let out = paths.erlang_shipment_directory();

    fs::mkdir(&out)?;

    // Reset the directories to ensure we have a clean slate and no old code
    fs::delete_directory(&build)?;
    fs::delete_directory(&out)?;

    // Build project in production mode
    let built = crate::build::main(
        paths,
        Options {
            root_target_support: TargetSupport::Enforced,
            warnings_as_errors: false,
            codegen: Codegen::All,
            compile: Compile::All,
            mode,
            target: Some(target),
            no_print_progress: false,
        },
        crate::build::download_dependencies(paths, crate::cli::Reporter::new())?,
    )?;

    for entry in fs::read_dir(&build)?.filter_map(Result::ok) {
        let path = entry.path();

        // We are only interested in package directories
        if !path.is_dir() {
            continue;
        }

        let name = path.file_name().expect("Directory name");
        let build = build.join(name);
        let out = out.join(name);
        fs::mkdir(&out)?;

        // Copy desired package subdirectories
        for subdirectory in ["ebin", "priv", "include"] {
            let source = build.join(subdirectory);
            if source.is_dir() {
                let source = fs::canonicalise(&source)?;
                let out = out.join(subdirectory);
                fs::copy_dir(source, &out)?;
            }
        }
    }

    // PowerShell entry point script.
    write_entrypoint_script(
        &out.join(ENTRYPOINT_FILENAME_POWERSHELL),
        ENTRYPOINT_TEMPLATE_POWERSHELL,
        &built.root_package.config.name,
    )?;

    // POSIX Shell entry point script.
    write_entrypoint_script(
        &out.join(ENTRYPOINT_FILENAME_POSIX_SHELL),
        ENTRYPOINT_TEMPLATE_POSIX_SHELL,
        &built.root_package.config.name,
    )?;

    crate::cli::print_exported(&built.root_package.config.name);

    println!(
        "
Your Erlang shipment has been generated to {out}.

It can be copied to a compatible server with Erlang installed and run with
one of the following scripts:
    - {ENTRYPOINT_FILENAME_POWERSHELL} (PowerShell script)
    - {ENTRYPOINT_FILENAME_POSIX_SHELL} (POSIX Shell script)
",
    );

    Ok(())
}

fn write_entrypoint_script(
    entrypoint_output_path: &Utf8PathBuf,
    entrypoint_template_path: &str,
    package_name: &str,
) -> Result<()> {
    let text = entrypoint_template_path.replace("$PACKAGE_NAME_FROM_GLEAM", package_name);
    fs::write(entrypoint_output_path, &text)?;
    fs::make_executable(entrypoint_output_path)?;
    Ok(())
}

pub fn hex_tarball(paths: &ProjectPaths) -> Result<()> {
    let mut config = crate::config::root_config(paths)?;
    let data: Vec<u8> = crate::publish::build_hex_tarball(paths, &mut config)?;

    let path = paths.build_export_hex_tarball(&config.name, &config.version.to_string());
    fs::write_bytes(&path, &data)?;
    println!(
        "
Your hex tarball has been generated in {}.
",
        path
    );
    Ok(())
}

pub fn javascript_prelude() -> Result<()> {
    print!("{}", gleam_core::javascript::PRELUDE);
    Ok(())
}

pub fn typescript_prelude() -> Result<()> {
    print!("{}", gleam_core::javascript::PRELUDE_TS_DEF);
    Ok(())
}

pub fn package_interface(paths: &ProjectPaths, out: Utf8PathBuf) -> Result<()> {
    // Build the project
    let mut built = crate::build::main(
        paths,
        Options {
            mode: Mode::Prod,
            target: None,
            codegen: Codegen::None,
            compile: Compile::All,
            warnings_as_errors: false,
            root_target_support: TargetSupport::Enforced,
            no_print_progress: false,
        },
        crate::build::download_dependencies(paths, crate::cli::Reporter::new())?,
    )?;
    built.root_package.attach_doc_and_module_comments();

    let out = gleam_core::docs::generate_json_package_interface(
        out,
        &built.root_package,
        &built.module_interfaces,
    );
    fs::write_outputs_under(&[out], paths.root())?;
    Ok(())
}

pub fn package_information(paths: &ProjectPaths, out: Utf8PathBuf) -> Result<()> {
    let config = crate::config::root_config(paths)?;
    let out = gleam_core::docs::generate_json_package_information(out, config);
    fs::write_outputs_under(&[out], paths.root())?;
    Ok(())
}



/// Bare-file mode: a `.gleam` file given instead of running inside a
/// project. A throwaway project is scaffolded in the system temp
/// directory (stable per file name, so incremental builds cache), with
/// the zig-target `gleam_stdlib` fork as a path dependency — found via
/// $GLEAM_ZIG_STDLIB or a `gleam-stdlib/` directory in an ancestor of
/// the file or the working directory.
pub fn bare_file_project(file: &camino::Utf8Path) -> Result<ProjectPaths> {
    if file.extension() != Some("gleam") {
        return Err(gleam_core::Error::FileIo {
            kind: gleam_core::error::FileKind::File,
            action: gleam_core::error::FileIoAction::Open,
            path: file.to_path_buf(),
            err: Some("expected a .gleam file".into()),
        });
    }
    let source = crate::fs::read(file)?;
    let stem = file
        .file_stem()
        .unwrap_or("main")
        .to_lowercase()
        .replace('-', "_");
    let mut module: String = stem
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_')
        .collect();
    if module.is_empty() || module.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        module = format!("m{module}");
    }

    let stdlib = find_zig_stdlib(file)?;

    let root = camino::Utf8PathBuf::from_path_buf(std::env::temp_dir())
        .expect("temp dir is utf-8")
        .join("gleam-bare-file")
        .join(&module);
    crate::fs::mkdir(&root.join("src"))?;
    // Canonicalise (macOS tempdirs live behind a /var symlink): path
    // dependencies get relativised against the project root in the
    // manifest, and a symlinked root makes those paths unresolvable on
    // the next run.
    let root = camino::Utf8PathBuf::from_path_buf(
        std::fs::canonicalize(root.as_std_path()).expect("temp project just created"),
    )
    .expect("temp dir is utf-8");
    crate::fs::write(
        &root.join("gleam.toml"),
        &format!(
            "name = \"{module}\"\nversion = \"1.0.0\"\n\n[dependencies]\ngleam_stdlib = {{ path = \"{stdlib}\" }}\n"
        ),
    )?;
    crate::fs::write(&root.join("src").join(format!("{module}.gleam")), &source)?;
    Ok(ProjectPaths::new(root))
}

fn find_zig_stdlib(file: &camino::Utf8Path) -> Result<camino::Utf8PathBuf> {
    if let Ok(path) = std::env::var("GLEAM_ZIG_STDLIB") {
        return Ok(camino::Utf8PathBuf::from(path));
    }
    let mut starts: Vec<camino::Utf8PathBuf> = Vec::new();
    let absolute = if file.is_absolute() {
        file.to_path_buf()
    } else {
        camino::Utf8PathBuf::from_path_buf(
            std::env::current_dir().expect("current directory exists"),
        )
        .expect("cwd is utf-8")
        .join(file)
    };
    if let Some(parent) = absolute.parent() {
        starts.push(parent.to_path_buf());
    }
    starts.push(
        camino::Utf8PathBuf::from_path_buf(
            std::env::current_dir().expect("current directory exists"),
        )
        .expect("cwd is utf-8"),
    );
    for start in starts {
        let mut dir = Some(start.as_path());
        while let Some(current) = dir {
            let candidate = current.join("gleam-stdlib");
            if candidate.join("gleam.toml").is_file() {
                return Ok(candidate);
            }
            dir = current.parent();
        }
    }
    Err(gleam_core::Error::FileIo {
        kind: gleam_core::error::FileKind::Directory,
        action: gleam_core::error::FileIoAction::Open,
        path: camino::Utf8PathBuf::from("gleam-stdlib"),
        err: Some(
            "bare-file mode needs the zig-target gleam_stdlib fork: set \
GLEAM_ZIG_STDLIB or keep a gleam-stdlib/ checkout in an ancestor directory"
                .into(),
        ),
    })
}

/// Export the whole build as one runnable zig source file: every module
/// and native file wrapped in a namespace struct, the prelude inlined
/// once, and an entrypoint at the bottom. Anyone with a zig toolchain
/// can `zig run` the result with no gleam installed.
pub fn zig_source(paths: &ProjectPaths, output: Option<Utf8PathBuf>) -> Result<()> {
    let target = Target::Zig;
    let mode = Mode::Prod;

    let manifest = crate::build::download_dependencies(paths, crate::cli::Reporter::new())?;
    let build_options = Options {
        root_target_support: TargetSupport::Enforced,
        warnings_as_errors: false,
        codegen: Codegen::All,
        compile: Compile::All,
        mode,
        target: Some(target),
        no_print_progress: false,
    };
    let built = crate::build::main(paths, build_options, manifest)?;
    let package_name = built.root_package.config.name.clone();
    let _ = built.get_main_function(&package_name, target)?;

    let build_root = paths.build_directory_for_target(mode, target);

    // Collect every .zig file under the build root (generated modules and
    // copied native files alike), keyed by root-relative path sans
    // extension. The prelude and any leftover entrypoints are handled
    // separately.
    let mut files: Vec<(String, String)> = Vec::new();
    let mut pending = vec![build_root.clone()];
    while let Some(dir) = pending.pop() {
        let entries = std::fs::read_dir(dir.as_std_path()).map_err(|error| {
            gleam_core::Error::FileIo {
                kind: gleam_core::error::FileKind::Directory,
                action: gleam_core::error::FileIoAction::Read,
                path: dir.clone(),
                err: Some(error.to_string()),
            }
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| gleam_core::Error::FileIo {
                kind: gleam_core::error::FileKind::Directory,
                action: gleam_core::error::FileIoAction::Read,
                path: dir.clone(),
                err: Some(error.to_string()),
            })?;
            let entry_path = Utf8PathBuf::from_path_buf(entry.path())
                .expect("build paths are utf-8");
            if entry_path.is_dir() {
                pending.push(entry_path);
                continue;
            }
            if entry_path.extension() != Some("zig") {
                continue;
            }
            let relative = entry_path
                .strip_prefix(&build_root)
                .expect("walked file is under the build root")
                .as_str()
                .trim_end_matches(".zig")
                .to_string();
            if relative == "prelude" || relative.starts_with("entrypoint@") {
                continue;
            }
            let text = crate::fs::read(&entry_path)?;
            files.push((relative, text));
        }
    }
    files.sort();

    let prelude = crate::fs::read(build_root.join("prelude.zig"))?;

    // `a/b/../c` -> `a/c`, resolving the dots @import climbs with.
    fn normalise(path: &str) -> String {
        let mut parts: Vec<&str> = Vec::new();
        for part in path.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    let _ = parts.pop();
                }
                part => parts.push(part),
            }
        }
        parts.join("/")
    }

    fn mangled(key: &str) -> String {
        if key == "prelude" {
            "@\"gleam$prelude\"".to_string()
        } else {
            format!("@\"gleam${key}\"")
        }
    }

    // Rewrite every relative @import in a file to the wrapped namespace
    // it resolves to; std and builtin imports pass through.
    fn rewrite_imports(text: &str, file_key: &str) -> String {
        let directory = match file_key.rfind('/') {
            Some(index) => &file_key[..index],
            None => "",
        };
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("@import(\"") {
            let after = &rest[start + 9..];
            let Some(end) = after.find('"') else { break };
            let import = &after[..end];
            out.push_str(&rest[..start]);
            if import == "std" || import == "builtin" {
                out.push_str(&rest[start..start + 9 + end + 2]);
            } else {
                let resolved = normalise(&format!("{directory}/{import}"));
                let key = resolved.trim_end_matches(".zig");
                out.push_str(&mangled(key));
            }
            rest = &rest[start + 9 + end + 2..];
        }
        out.push_str(rest);
        out
    }

    let mut single = String::new();
    single.push_str(&format!(
        "// Single-file export of the Gleam project `{package_name}` (zig target).\n// Generated by `gleam export zig-source`; run with `zig run <file>`.\n\nconst @\"gleam$std\" = @import(\"std\");\n\nconst @\"gleam$prelude\" = struct {{\n{prelude}}};\n\n"
    ));
    for (key, text) in &files {
        single.push_str(&format!(
            "const {} = struct {{\n{}}};\n\n",
            mangled(key),
            rewrite_imports(text, key)
        ));
    }
    single.push_str(&format!(
        "pub fn main(init: @\"gleam$std\".process.Init.Minimal) void {{\n    @\"gleam$prelude\".process_args = init.args;\n    @\"gleam$prelude\".process_environ = init.environ;\n    @\"gleam$prelude\".drop({}.@\"main\"());\n    @\"gleam$prelude\".leakCheckExit();\n}}\n",
        mangled(&format!("{package_name}/{package_name}"))
    ));

    let output = output.unwrap_or_else(|| Utf8PathBuf::from(format!("{package_name}.zig")));
    fs::write(&output, &single)?;
    println!("Wrote {output}");
    Ok(())
}

/// Build a native executable via the zig target.
///
/// `optimize` maps onto zig's modes: `debug` compiles roughly twice as
/// fast and keeps the leak gate on; `fast` (the default) is the
/// published configuration.
#[derive(clap::ValueEnum, Clone, Debug, PartialEq, Eq)]
pub enum ZigOptimize {
    Debug,
    Safe,
    Fast,
    Small,
}

impl ZigOptimize {
    fn zig_flag(&self) -> &'static str {
        match self {
            ZigOptimize::Debug => "-ODebug",
            ZigOptimize::Safe => "-OReleaseSafe",
            ZigOptimize::Fast => "-OReleaseFast",
            ZigOptimize::Small => "-OReleaseSmall",
        }
    }
}

pub fn zig_executable(
    paths: &ProjectPaths,
    output: Option<Utf8PathBuf>,
    cross_target: Option<String>,
    optimize: ZigOptimize,
) -> Result<()> {
    let target = Target::Zig;
    let mode = Mode::Prod;

    let manifest = crate::build::download_dependencies(paths, crate::cli::Reporter::new())?;
    let build_options = Options {
        root_target_support: TargetSupport::Enforced,
        warnings_as_errors: false,
        codegen: Codegen::All,
        compile: Compile::All,
        mode,
        target: Some(target),
        no_print_progress: false,
    };
    let built = crate::build::main(paths, build_options, manifest)?;
    let package_name = built.root_package.config.name.clone();

    // The executable calls main; fail early if there is none.
    let _ = built.get_main_function(&package_name, target)?;

    // The entrypoint lives at the target build root so every generated
    // module is importable.
    let entrypoint = format!(
        r#"const std = @import("std");
const P = @import("prelude.zig");
const module = @import("{package_name}/{package_name}.zig");
pub fn main(init: std.process.Init.Minimal) void {{
    P.process_args = init.args;
    P.process_environ = init.environ;
    P.drop(module.@"main"());
    P.leakCheckExit();
}}
"#
    );
    let build_root = paths.build_directory_for_target(mode, target);
    let entrypoint_path = build_root.join(format!("entrypoint@{package_name}.zig"));
    fs::write(&entrypoint_path, &entrypoint)?;

    let output = output.unwrap_or_else(|| Utf8PathBuf::from(package_name.as_str()));
    let zig = crate::zig_toolchain::ensure_zig()?;
    let mut command = std::process::Command::new(&zig);
    let _ = command
        .arg("build-exe")
        .arg(entrypoint_path.as_str())
        .arg(optimize.zig_flag())
        .arg(format!("-femit-bin={output}"));
    if let Some(cross_target) = &cross_target {
        // Hermetic cross-compilation, e.g. x86_64-linux or aarch64-windows.
        let _ = command.arg("-target").arg(cross_target);
    }
    let status = command.status()
        .map_err(|error| gleam_core::Error::ShellCommand {
            program: zig.to_string(),
            reason: gleam_core::error::ShellCommandFailureReason::IoError(error.kind()),
        })?;
    if !status.success() {
        return Err(gleam_core::Error::ShellCommand {
            program: zig.into_string(),
            reason: gleam_core::error::ShellCommandFailureReason::Unknown,
        });
    }
    println!("Wrote {output}");
    Ok(())
}
