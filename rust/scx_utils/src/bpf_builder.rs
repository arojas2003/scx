// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2.

use crate::clang_info::ClangInfo;
use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use glob::glob;
use libbpf_cargo::SkeletonBuilder;
use libbpf_rs::Linker;
use std::collections::BTreeSet;
use std::env;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug)]
/// # Build helpers for sched_ext schedulers with Rust userspace component
///
/// This is to be used from `build.rs` of a cargo project which implements a
/// [sched_ext](https://github.com/sched-ext/scx) scheduler with C BPF
/// component and Rust userspace component. `BpfBuilder` provides everything
/// necessary to build the BPF component and generate Rust bindings.
/// BpfBuilder provides the followings.
///
/// 1. *`vmlinux.h` and other common BPF header files*
///
/// All sched_ext BPF implementations require `vmlinux.h` and many make use
/// of common constructs such as
/// [`user_exit_info`](https://github.com/sched-ext/scx/blob/main/scheds/include/common/user_exit_info.h).
/// `BpfBuilder` makes these headers available when compiling BPF source
/// code and generating bindings for it. The included headers can be browsed
/// at <https://github.com/sched-ext/scx/tree/main/scheds/include>.
///
/// These headers can be superseded using environment variables which will
/// be discussed later.
///
/// 2. *Header bindings using `bindgen`*
///
/// If enabled with `.enable_intf()`, the input `.h` file is processed by
/// `bindgen` to generate Rust bindings. This is useful in establishing
/// shared constants and data types between the BPF and user components.
///
/// Note that the types generated with `bindgen` are different from the
/// types used by the BPF skeleton even when they are the same types in BPF.
/// This is a source of ugliness and we are hoping to address it by
/// improving `libbpf-cargo` in the future.
///
/// 3. *BPF compilation and generation of the skeleton and its bindings*
///
/// If enabled with `.enable_skel()`, the input `.bpf.c` file is compiled
/// and its skeleton and bindings are generated using `libbpf-cargo`.
///
/// ## An Example
///
/// This section shows how `BpfBuilder` can be used in an example project.
/// For a concrete example, take a look at
/// [`scx_rusty`](https://github.com/sched-ext/scx/tree/main/scheds/rust/scx_rusty).
///
/// A minimal source tree using all features would look like the following:
///
/// ```text
/// scx_hello_world
/// |-- Cargo.toml
/// |-- build.rs
/// \-- src
///     |-- main.rs
///     |-- bpf_intf.rs
///     |-- bpf_skel.rs
///     \-- bpf
///         |-- intf.h
///         \-- main.c
/// ```
///
/// The following three files would contain the actual implementation:
///
/// - `src/main.rs`: Rust userspace component which loads the BPF blob and
/// interacts it using the generated bindings.
///
/// - `src/bpf/intf.h`: C header file definining constants and structs
/// that will be used by both the BPF and userspace components.
///
/// - `src/bpf/main.c`: C source code implementing the BPF component -
/// including `struct sched_ext_ops`.
///
/// And then there are boilerplates to generate the bindings and make them
/// available as modules to `main.rs`.
///
/// - `Cargo.toml`: Includes `scx_utils` in the `[build-dependencies]`
/// section.
///
/// - `build.rs`: Uses `scx_utils::BpfBuilder` to build and generate
/// bindings for the BPF component. For this project, it can look like the
/// following.
///
/// ```should_panic
/// fn main() {
///     scx_utils::BpfBuilder::new()
///         .unwrap()
///         .enable_intf("src/bpf/intf.h", "bpf_intf.rs")
///         .enable_skel("src/bpf/main.bpf.c", "bpf")
///         .build()
///         .unwrap();
/// }
/// ```
///
/// - `bpf_intf.rs`: Import the bindings generated by `bindgen` into a
/// module. Above, we told `.enable_intf()` to generate the bindings into
/// `bpf_intf.rs`, so the file would look like the following. The `allow`
/// directives are useful if the header is including `vmlinux.h`.
///
/// ```ignore
/// #![allow(non_upper_case_globals)]
/// #![allow(non_camel_case_types)]
/// #![allow(non_snake_case)]
/// #![allow(dead_code)]
///
/// include!(concat!(env!("OUT_DIR"), "/bpf_intf.rs"));
/// ```
///
/// - `bpf_skel.rs`: Import the BPF skeleton bindings generated by
/// `libbpf-cargo` into a module. Above, we told `.enable_skel()` to use the
/// skeleton name `bpf`, so the file would look like the following.
///
/// ```ignore
/// include!(concat!(env!("OUT_DIR"), "/bpf_skel.rs"));
/// ```
///
/// ## Compiler Flags and Environment Variables
///
/// BPF being its own CPU architecture and independent runtime environment,
/// build environment and steps are already rather complex. The need to
/// interface between two different languages - C and Rust - adds further
/// complexities. `BpfBuilder` automates most of the process. The determined
/// build environment is recorded in the `build.rs` output and can be
/// obtained with a command like the following:
///
/// ```text
/// $ grep '^scx_utils:clang=' target/release/build/scx_rusty-*/output
/// ```
///
/// While the automatic settings should work most of the time, there can be
/// times when overriding them is necessary. The following environment
/// variables can be used to customize the build environment.
///
/// - `BPF_CLANG`: The clang command to use. (Default: `clang`)
///
/// - `BPF_CFLAGS`: Compiler flags to use when building BPF source code. If
///   specified, the flags from this variable are the only flags passed to
///   the compiler. `BpfBuilder` won't generate any flags including `-I`
///   flags for the common header files and other `CFLAGS` related variables
///   are ignored.
///
/// - `BPF_BASE_CFLAGS`: Override the non-include part of cflags.
///
/// - `BPF_EXTRA_CFLAGS_PRE_INCL`: Add cflags before the automic include
///   search path options. Header files in the search paths added by this
///   variable will supercede the automatic ones.
///
/// - `BPF_EXTRA_CFLAGS_POST_INCL`: Add cflags after the automic include
///   search path options. Header paths added by this variable will be
///   searched only if the target header file can't be found in the
///   automatic header paths.
///
/// - `RUSTFLAGS`: This is a generic `cargo` flag and can be useful for
///   specifying extra linker flags.
///
/// A common case for using the above flags is using the latest `libbpf`
/// from the kernel tree. Let's say the kernel tree is at `$KERNEL` and
/// `libbpf`. The following builds `libbpf` shipped with the kernel:
///
/// ```test
/// $ cd $KERNEL
/// $ make -C tools/bpf/bpftool
/// ```
///
/// To link the scheduler against the resulting `libbpf`:
///
/// ```test
/// $ env BPF_EXTRA_CFLAGS_POST_INCL=$KERNEL/tools/bpf/bpftool/libbpf/include \
///   RUSTFLAGS="-C link-args=-lelf -C link-args=-lz -C link-args=-lzstd \
///   -L$KERNEL/tools/bpf/bpftool/libbpf" cargo build --release
/// ```
pub struct BpfBuilder {
    clang: ClangInfo,
    cflags: Vec<String>,
    out_dir: PathBuf,
    sources: BTreeSet<String>,

    intf_input_output: Option<(String, String)>,
    skel_input_name: Option<(String, String)>,
}

impl BpfBuilder {
    const BPF_H_TAR: &'static [u8] = include_bytes!(concat!(env!("OUT_DIR"), "/bpf_h.tar"));

    fn install_bpf_h<P: AsRef<Path>>(dest: P) -> Result<()> {
        let mut ar = tar::Archive::new(Self::BPF_H_TAR);
        ar.unpack(dest)?;
        Ok(())
    }

    fn determine_cflags<P>(clang: &ClangInfo, out_dir: P) -> Result<Vec<String>>
    where
        P: AsRef<Path> + std::fmt::Debug,
    {
        let bpf_h = out_dir
            .as_ref()
            .join("scx_utils-bpf_h")
            .to_str()
            .ok_or(anyhow!(
                "{:?}/scx_utils-bph_h can't be converted to str",
                &out_dir
            ))?
            .to_string();
        Self::install_bpf_h(&bpf_h)?;

        let mut cflags = Vec::<String>::new();

        cflags.append(&mut match env::var("BPF_BASE_CFLAGS") {
            Ok(v) => v.split_whitespace().map(|x| x.into()).collect(),
            _ => clang.determine_base_cflags()?,
        });

        cflags.append(&mut match env::var("BPF_EXTRA_CFLAGS_PRE_INCL") {
            Ok(v) => v.split_whitespace().map(|x| x.into()).collect(),
            _ => vec![],
        });

        cflags.push(format!(
            "-I{}/arch/{}",
            &bpf_h,
            &clang.kernel_target().unwrap()
        ));
        cflags.push(format!("-I{}", &bpf_h));
        cflags.push(format!("-I{}/bpf-compat", &bpf_h));

        cflags.append(&mut match env::var("BPF_EXTRA_CFLAGS_POST_INCL") {
            Ok(v) => v.split_whitespace().map(|x| x.into()).collect(),
            _ => vec![],
        });

        Ok(cflags)
    }

    /// Create a new `BpfBuilder` struct. Call `enable` and `set` methods to
    /// configure and `build` method to compile and generate bindings. See
    /// the struct documentation for details.
    pub fn new() -> Result<Self> {
        let out_dir = PathBuf::from(env::var("OUT_DIR")?);

        let clang = ClangInfo::new()?;
        let cflags = match env::var("BPF_CFLAGS") {
            Ok(v) => v.split_whitespace().map(|x| x.into()).collect(),
            _ => Self::determine_cflags(&clang, &out_dir)?,
        };

        println!("scx_utils:clang={:?} {:?}", &clang, &cflags);

        Ok(Self {
            clang,
            cflags,
            out_dir,

            sources: BTreeSet::new(),
            intf_input_output: None,
            skel_input_name: None,
        })
    }

    /// Enable generation of header bindings using `bindgen`. `@input` is
    /// the `.h` file defining the constants and types to be shared between
    /// BPF and Rust components. `@output` is the `.rs` file to be
    /// generated.
    pub fn enable_intf(&mut self, input: &str, output: &str) -> &mut Self {
        self.intf_input_output = Some((input.into(), output.into()));
        self
    }

    /// Enable compilation of BPF code and generation of the skeleton and
    /// its Rust bindings. `@input` is the `.bpf.c` file containing the BPF
    /// source code and `@output` is the `.rs` file to be generated.
    pub fn enable_skel(&mut self, input: &str, name: &str) -> &mut Self {
        self.skel_input_name = Some((input.into(), name.into()));
        self.sources.insert(input.into());

        self
    }

    fn input_insert_deps(&self, deps: &mut BTreeSet<String>) -> () {
        let (input, _) = match &self.intf_input_output {
            Some(pair) => pair,
            None => return (),
        };

        // Tell cargo to invalidate the built crate whenever the wrapper changes
        deps.insert(input.to_string());
    }

    fn bindgen_bpf_intf(&self) -> Result<()> {
        let (input, output) = match &self.intf_input_output {
            Some(pair) => pair,
            None => return Ok(()),
        };

        // The bindgen::Builder is the main entry point to bindgen, and lets
        // you build up options for the resulting bindings.
        let bindings = bindgen::Builder::default()
            // Should run clang with the same -I options as BPF compilation.
            .clang_args(
                self.cflags
                    .iter()
                    .chain(["-target".into(), "bpf".into()].iter()),
            )
            // The input header we would like to generate bindings for.
            .header(input)
            // Tell cargo to invalidate the built crate whenever any of the
            // included header files changed.
            .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
            .generate()
            .context("Unable to generate bindings")?;

        bindings
            .write_to_file(self.out_dir.join(output))
            .context("Couldn't write bindings")
    }

    pub fn add_source(&mut self, input: &str) -> &mut Self {
        self.sources.insert(input.into());
        self
    }

    pub fn compile_link_gen(&self) -> Result<()> {
        let (_, name) = match &self.skel_input_name {
            Some(pair) => pair,
            None => return Ok(()),
        };

        let linkobj = self.out_dir.join(format!("{}.bpf.o", name));
        let mut linker = Linker::new(&linkobj)?;

        for filename in self.sources.iter() {
            let obj = self.out_dir.join(name.replace(".bpf.c", ".bpf.o"));

            SkeletonBuilder::new()
                .debug(true)
                .source(filename)
                .obj(&obj)
                .clang(&self.clang.clang)
                .clang_args(&self.cflags)
                .build()?;

            linker.add_file(&obj)?;
        }

        linker.link()?;

        self.bindgen_bpf_intf()?;

        let skel_path = self.out_dir.join(format!("{}_skel.rs", name));

        SkeletonBuilder::new()
            .obj(&linkobj)
            .clang(&self.clang.clang)
            .clang_args(&self.cflags)
            .generate(&skel_path)?;

        self.gen_cargo_reruns(None)?;

        Ok(())
    }

    fn gen_bpf_skel(&self, deps: &mut BTreeSet<String>) -> Result<()> {
        let (input, name) = match &self.skel_input_name {
            Some(pair) => pair,
            None => return Ok(()),
        };

        let obj = self.out_dir.join(format!("{}.bpf.o", name));
        let skel_path = self.out_dir.join(format!("{}_skel.rs", name));

        let output = SkeletonBuilder::new()
            .source(input)
            .obj(&obj)
            .clang(&self.clang.clang)
            .clang_args(&self.cflags)
            .build_and_generate(&skel_path)?;

        for line in String::from_utf8_lossy(output.stderr()).lines() {
            println!("cargo:warning={}", line);
        }

        let c_path = PathBuf::from(input);
        let dir = c_path
            .parent()
            .ok_or(anyhow!("Source {:?} doesn't have parent dir", c_path))?
            .to_str()
            .ok_or(anyhow!("Parent dir of {:?} isn't a UTF-8 string", c_path))?;

        for path in glob(&format!("{}/*.[hc]", dir))?.filter_map(Result::ok) {
            deps.insert(
                path.to_str()
                    .ok_or(anyhow!("Path {:?} is not a valid string", path))?
                    .to_string(),
            );
        }

        Ok(())
    }

    fn gen_cargo_reruns(&self, dependencies: Option<&BTreeSet<String>>) -> Result<()> {
        println!("cargo:rerun-if-env-changed=BPF_CLANG");
        println!("cargo:rerun-if-env-changed=BPF_CFLAGS");
        println!("cargo:rerun-if-env-changed=BPF_BASE_CFLAGS");
        println!("cargo:rerun-if-env-changed=BPF_EXTRA_CFLAGS_PRE_INCL");
        println!("cargo:rerun-if-env-changed=BPF_EXTRA_CFLAGS_POST_INCL");
        match dependencies {
            Some(deps) => {
                for dep in deps.iter() {
                    println!("cargo:rerun-if-changed={}", dep);
                }
            }

            None => (),
        };

        for source in self.sources.iter() {
            println!("cargo:rerun-if-changed={}", source);
        }

        Ok(())
    }

    /// Build and generate the enabled bindings.
    pub fn build(&self) -> Result<()> {
        let mut deps = BTreeSet::new();

        self.input_insert_deps(&mut deps);

        self.bindgen_bpf_intf()?;
        self.gen_bpf_skel(&mut deps)?;
        self.gen_cargo_reruns(Some(&deps))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use regex::Regex;
    use sscanf::sscanf;

    use crate::builder::ClangInfo;

    #[test]
    fn test_bpf_builder_new() {
        let res = super::BpfBuilder::new();
        assert!(res.is_ok(), "Failed to create BpfBuilder ({:?})", &res);
    }

    #[test]
    fn test_vmlinux_h_ver_sha1() {
        let clang_info = ClangInfo::new().unwrap();

        let mut ar = tar::Archive::new(super::BpfBuilder::BPF_H_TAR);
        let mut found = false;

        let pattern = Regex::new(r"arch\/.*\/vmlinux-.*.h").unwrap();

        for entry in ar.entries().unwrap() {
            let entry = entry.unwrap();
            let file_name = entry.header().path().unwrap();
            let file_name_str = file_name.to_string_lossy().to_owned();
            if file_name_str.contains(&clang_info.kernel_target().unwrap()) {
                found = true;
            }
            if !pattern.find(&file_name_str).is_some() {
                continue;
            }

            println!("checking {file_name_str}");

            let (arch, ver, sha1) =
                sscanf!(file_name_str, "arch/{String}/vmlinux-v{String}-g{String}.h").unwrap();
            println!(
                "vmlinux.h: arch={:?} ver={:?} sha1={:?}",
                &arch, &ver, &sha1,
            );

            assert!(regex::Regex::new(r"^([1-9][0-9]*\.[1-9][0-9][a-z0-9-]*)$")
                .unwrap()
                .is_match(&ver));
            assert!(regex::Regex::new(r"^[0-9a-z]{12}$")
                .unwrap()
                .is_match(&sha1));
        }

        assert!(found);
    }
}
