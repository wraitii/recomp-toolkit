use std::path::Path;

#[path = "src/shader/opcodes.rs"]
mod opcodes;
#[path = "src/shader/uber/abi.rs"]
mod uber_abi;
#[path = "src/shader/uber/source.rs"]
mod uber_source;

fn interpreter_source() {
    let source = uber_source::source();
    let module = naga::front::wgsl::parse_str(&source)
        .unwrap_or_else(|error| panic!("{}", error.emit_to_string(&source)));
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::empty(),
    )
    .validate(&module)
    .unwrap_or_else(|error| panic!("{}", error.emit_to_string(&source)));
    let mut layouts = naga::proc::Layouter::default();
    layouts
        .update(module.to_ctx())
        .expect("interpreter ABI layout");
    for (name, size) in [
        ("Program", uber_abi::PROGRAM_BYTES),
        ("Instruction", (uber_abi::INSTRUCTION_WORDS * 4) as u64),
        ("Definition", 32),
        ("VIn", uber_abi::VERTEX_STRIDE),
        ("VSP", 272),
        ("PSP", 64),
    ] {
        let (handle, _) = module
            .types
            .iter()
            .find(|(_, ty)| ty.name.as_deref() == Some(name))
            .expect(name);
        assert_eq!(
            u64::from(layouts[handle].size),
            size,
            "interpreter {name} ABI"
        );
    }
    let (_, varyings) = module
        .types
        .iter()
        .find(|(_, ty)| ty.name.as_deref() == Some("V2F"))
        .unwrap();
    let naga::TypeInner::Struct { members, .. } = &varyings.inner else {
        unreachable!()
    };
    assert_eq!(
        members
            .iter()
            .filter(|member| matches!(member.binding, Some(naga::Binding::Location { .. })))
            .count() as u32,
        uber_abi::INTER_STAGE_VARIABLES,
    );
    let components: u32 = members
        .iter()
        .filter(|member| matches!(member.binding, Some(naga::Binding::Location { .. })))
        .map(|member| layouts[member.ty].size / 4)
        .sum();
    assert_eq!(components, 42);
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(out.join("d3d9-interpreter.wgsl"), &source).unwrap();
    let mut options = naga::back::msl::Options {
        lang_version: (2, 3),
        fake_missing_bindings: false,
        force_loop_bounding: false,
        ..Default::default()
    };
    for name in ["vs_main", "ps_main"] {
        let mut resources = naga::back::msl::EntryPointResources::default();
        for (binding, slot) in [(0, 0), (1, 1), (2, 2), (3, 3), (36, 4), (37, 5)] {
            resources.resources.insert(
                naga::ResourceBinding { group: 0, binding },
                naga::back::msl::BindTarget {
                    buffer: Some(slot),
                    ..Default::default()
                },
            );
        }
        resources.resources.insert(
            naga::ResourceBinding {
                group: 0,
                binding: 4,
            },
            naga::back::msl::BindTarget {
                texture: Some(0),
                ..Default::default()
            },
        );
        resources.resources.insert(
            naga::ResourceBinding {
                group: 0,
                binding: 20,
            },
            naga::back::msl::BindTarget {
                sampler: Some(naga::back::msl::BindSamplerTarget::Resource(0)),
                ..Default::default()
            },
        );
        options.per_entry_point_map.insert(name.into(), resources);
    }
    let (msl, translation) = naga::back::msl::write_string(
        &module,
        &info,
        &options,
        &naga::back::msl::PipelineOptions::default(),
    )
    .expect("interpreter Metal translation");
    for entry in translation.entry_point_names {
        entry.expect("interpreter Metal entry point");
    }
    std::fs::write(out.join("d3d9-interpreter.metal"), msl).unwrap();
}

fn fingerprint(path: &Path, root: &Path, hash: &mut u64) {
    if path.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(path)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for entry in entries {
            fingerprint(&entry, root, hash);
        }
    } else {
        let name = path.strip_prefix(root).unwrap().to_string_lossy();
        let contents = std::fs::read(path).unwrap();
        for byte in name
            .bytes()
            .chain([0])
            .chain((contents.len() as u64).to_le_bytes())
            .chain(contents)
        {
            *hash = (*hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
    }
}

fn main() {
    interpreter_source();
    let root = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let mut hash = 0xcbf29ce484222325;
    for input in ["src", "build.rs", "Cargo.toml", "Cargo.lock"] {
        println!("cargo:rerun-if-changed={input}");
        fingerprint(&root.join(input), &root, &mut hash);
    }
    println!(
        "cargo:rustc-env=RECOMP_RENDERER_BUILD={hash:016x}-{}-{}",
        std::env::var("TARGET").unwrap(),
        std::env::var("PROFILE").unwrap()
    );
    cxx_build::bridge("src/bridge.rs").compile("d3d8-wgpu-cxx");
}
