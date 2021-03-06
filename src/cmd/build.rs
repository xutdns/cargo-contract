// Copyright 2018-2019 Parity Technologies (UK) Ltd.
// This file is part of ink!.
//
// ink! is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// ink! is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with ink!.  If not, see <http://www.gnu.org/licenses/>.

use std::{
    fs::metadata,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
};

use crate::{
    util,
    workspace::{ManifestPath, Workspace},
    Verbosity,
};
use anyhow::{Context, Result};
use cargo_metadata::Package;
use colored::Colorize;
use parity_wasm::elements::{External, MemoryType, Module, Section};

/// This is the maximum number of pages available for a contract to allocate.
const MAX_MEMORY_PAGES: u32 = 16;

/// Relevant metadata obtained from Cargo.toml.
#[derive(Debug)]
pub struct CrateMetadata {
    manifest_path: ManifestPath,
    cargo_meta: cargo_metadata::Metadata,
    package_name: String,
    root_package: Package,
    original_wasm: PathBuf,
    pub dest_wasm: PathBuf,
}

impl CrateMetadata {
    pub fn target_dir(&self) -> &Path {
        self.cargo_meta.target_directory.as_path()
    }
}

/// Parses the contract manifest and returns relevant metadata.
pub fn collect_crate_metadata(manifest_path: &ManifestPath) -> Result<CrateMetadata> {
    let (metadata, root_package_id) = crate::util::get_cargo_metadata(manifest_path)?;

    // Find the root package by id in the list of packages. It is logical error if the root
    // package is not found in the list.
    let root_package = metadata
        .packages
        .iter()
        .find(|package| package.id == root_package_id)
        .expect("The package is not found in the `cargo metadata` output")
        .clone();

    // Normalize the package name.
    let package_name = root_package.name.replace("-", "_");

    // {target_dir}/wasm32-unknown-unknown/release/{package_name}.wasm
    let mut original_wasm = metadata.target_directory.clone();
    original_wasm.push("wasm32-unknown-unknown");
    original_wasm.push("release");
    original_wasm.push(package_name.clone());
    original_wasm.set_extension("wasm");

    // {target_dir}/{package_name}.wasm
    let mut dest_wasm = metadata.target_directory.clone();
    dest_wasm.push(package_name.clone());
    dest_wasm.set_extension("wasm");

    let crate_metadata = CrateMetadata {
        manifest_path: manifest_path.clone(),
        cargo_meta: metadata,
        root_package: root_package.clone(),
        package_name,
        original_wasm,
        dest_wasm,
    };
    Ok(crate_metadata)
}

/// Builds the project in the specified directory, defaults to the current directory.
///
/// Uses [`cargo-xbuild`](https://github.com/rust-osdev/cargo-xbuild) for maximum optimization of
/// the resulting Wasm binary.
fn build_cargo_project(crate_metadata: &CrateMetadata, verbosity: Option<Verbosity>) -> Result<()> {
    util::assert_channel()?;

    // set RUSTFLAGS, read from environment var by cargo-xbuild
    std::env::set_var(
        "RUSTFLAGS",
        "-C link-arg=-z -C link-arg=stack-size=65536 -C link-arg=--import-memory",
    );

    let verbosity = verbosity.map(|v| match v {
        Verbosity::Verbose => xargo_lib::Verbosity::Verbose,
        Verbosity::Quiet => xargo_lib::Verbosity::Quiet,
    });

    let xbuild = |manifest_path: &ManifestPath| {
        let manifest_path = Some(manifest_path);
        let target = Some("wasm32-unknown-unknown");
        let target_dir = crate_metadata.target_dir();
        let other_args = [
            "--no-default-features",
            "--release",
            &format!("--target-dir={}", target_dir.to_string_lossy()),
        ];
        let args = xargo_lib::Args::new(target, manifest_path, verbosity, &other_args)
            .map_err(|e| anyhow::anyhow!("{}", e))
            .context("Creating xargo args")?;

        let config = xargo_lib::Config {
            sysroot_path: target_dir.join("sysroot"),
            memcpy: false,
            panic_immediate_abort: true,
        };

        let exit_status = xargo_lib::build(args, "build", Some(config))
            .map_err(|e| anyhow::anyhow!("{}", e))
            .context("Building with xbuild")?;
        if !exit_status.success() {
            anyhow::bail!("xbuild failed with status {}", exit_status)
        }
        Ok(())
    };

    Workspace::new(&crate_metadata.cargo_meta, &crate_metadata.root_package.id)?
        .with_root_package_manifest(|manifest| {
            manifest.with_removed_crate_type("rlib")?;
            Ok(())
        })?
        .using_temp(xbuild)?;

    Ok(())
}

/// Ensures the wasm memory import of a given module has the maximum number of pages.
///
/// Iterates over the import section, finds the memory import entry if any and adjusts the maximum
/// limit.
fn ensure_maximum_memory_pages(module: &mut Module, maximum_allowed_pages: u32) -> Result<()> {
    let mem_ty = module
        .import_section_mut()
        .and_then(|section| {
            section
                .entries_mut()
                .iter_mut()
                .find_map(|entry| match entry.external_mut() {
                    External::Memory(ref mut mem_ty) => Some(mem_ty),
                    _ => None,
                })
        })
        .context("Memory import is not found. Is --import-memory specified in the linker args")?;

    if let Some(requested_maximum) = mem_ty.limits().maximum() {
        // The module already has maximum, check if it is within the limit bail out.
        if requested_maximum > maximum_allowed_pages {
            anyhow::bail!(
                "The wasm module requires {} pages. The maximum allowed number of pages is {}",
                requested_maximum,
                maximum_allowed_pages,
            );
        }
    } else {
        let initial = mem_ty.limits().initial();
        *mem_ty = MemoryType::new(initial, Some(MAX_MEMORY_PAGES));
    }

    Ok(())
}

/// Strips all custom sections.
///
/// Presently all custom sections are not required so they can be stripped safely.
fn strip_custom_sections(module: &mut Module) {
    module.sections_mut().retain(|section| match section {
        Section::Custom(_) => false,
        Section::Name(_) => false,
        Section::Reloc(_) => false,
        _ => true,
    });
}

/// Performs required post-processing steps on the wasm artifact.
fn post_process_wasm(crate_metadata: &CrateMetadata) -> Result<()> {
    // Deserialize wasm module from a file.
    let mut module =
        parity_wasm::deserialize_file(&crate_metadata.original_wasm).context(format!(
            "Loading original wasm file '{}'",
            crate_metadata.original_wasm.display()
        ))?;

    // Perform optimization.
    //
    // In practice only tree-shaking is performed, i.e transitively removing all symbols that are
    // NOT used by the specified entrypoints.
    if pwasm_utils::optimize(&mut module, ["call", "deploy"].to_vec()).is_err() {
        anyhow::bail!("Optimizer failed");
    }
    ensure_maximum_memory_pages(&mut module, MAX_MEMORY_PAGES)?;
    strip_custom_sections(&mut module);

    parity_wasm::serialize_to_file(&crate_metadata.dest_wasm, module)?;
    Ok(())
}

/// Attempts to perform optional wasm optimization using `wasm-opt`.
///
/// The intention is to reduce the size of bloated wasm binaries as a result of missing
/// optimizations (or bugs?) between Rust and Wasm.
///
/// This step depends on the `wasm-opt` tool being installed. If it is not the build will still
/// succeed, and the user will be encouraged to install it for further optimizations.
fn optimize_wasm(crate_metadata: &CrateMetadata) -> Result<()> {
    // check `wasm-opt` installed
    if which::which("wasm-opt").is_err() {
        println!(
            "{}",
            "wasm-opt is not installed. Install this tool on your system in order to \n\
             reduce the size of your contract's Wasm binary. \n\
             See https://github.com/WebAssembly/binaryen#tools"
                .bright_yellow()
        );
        return Ok(());
    }

    let mut optimized = crate_metadata.dest_wasm.clone();
    optimized.set_file_name(format!("{}-opt.wasm", crate_metadata.package_name));

    let output = Command::new("wasm-opt")
        .arg(crate_metadata.dest_wasm.as_os_str())
        .arg("-O3") // execute -O3 optimization passes (spends potentially a lot of time optimizing)
        .arg("-o")
        .arg(optimized.as_os_str())
        .output()?;

    if !output.status.success() {
        // Dump the output streams produced by wasm-opt into the stdout/stderr.
        io::stdout().write_all(&output.stdout)?;
        io::stderr().write_all(&output.stderr)?;
        anyhow::bail!("wasm-opt optimization failed");
    }

    let original_size = metadata(&crate_metadata.dest_wasm)?.len() as f64 / 1000.0;
    let optimized_size = metadata(&optimized)?.len() as f64 / 1000.0;
    println!(
        " Original wasm size: {:.1}K, Optimized: {:.1}K",
        original_size, optimized_size
    );

    // overwrite existing destination wasm file with the optimised version
    std::fs::rename(&optimized, &crate_metadata.dest_wasm)?;
    Ok(())
}

/// Executes build of the smart-contract which produces a wasm binary that is ready for deploying.
///
/// It does so by invoking build by cargo and then post processing the final binary.
pub(crate) fn execute_build(
    manifest_path: ManifestPath,
    verbosity: Option<Verbosity>,
) -> Result<String> {
    println!(
        " {} {}",
        "[1/4]".bold(),
        "Collecting crate metadata".bright_green().bold()
    );
    let crate_metadata = collect_crate_metadata(&manifest_path)?;
    println!(
        " {} {}",
        "[2/4]".bold(),
        "Building cargo project".bright_green().bold()
    );
    build_cargo_project(&crate_metadata, verbosity)?;
    println!(
        " {} {}",
        "[3/4]".bold(),
        "Post processing wasm file".bright_green().bold()
    );
    post_process_wasm(&crate_metadata)?;
    println!(
        " {} {}",
        "[4/4]".bold(),
        "Optimizing wasm file".bright_green().bold()
    );
    optimize_wasm(&crate_metadata)?;

    Ok(format!(
        "\nYour contract is ready. You can find it here:\n{}",
        crate_metadata.dest_wasm.display().to_string().bold()
    ))
}

#[cfg(feature = "test-ci-only")]
#[cfg(test)]
mod tests {
    use crate::{cmd::execute_new, util::tests::with_tmp_dir, workspace::ManifestPath};

    #[test]
    fn build_template() {
        with_tmp_dir(|path| {
            execute_new("new_project", Some(path)).expect("new project creation failed");
            let manifest_path =
                ManifestPath::new(&path.join("new_project").join("Cargo.toml")).unwrap();
            super::execute_build(manifest_path, None).expect("build failed");
        });
    }
}
