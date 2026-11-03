use quote::{format_ident, quote};
use std::{env, fs, path::PathBuf};
use syn::{ImplItem, Item, Type};

fn main() {
    println!("cargo:rerun-if-env-changed=HANDY_FORMAL_SOURCE_ROOT");
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let root = env::var_os("HANDY_FORMAL_SOURCE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.ancestors().nth(4).unwrap().into());
    let path = root.join("src-tauri/src/engine_supervisor/supervisor.rs");
    println!("cargo:rerun-if-changed={}", path.display());
    let file = syn::parse_file(&fs::read_to_string(path).unwrap()).unwrap();
    let mut tokens = quote! {};
    for name in [
        "LoadSpec",
        "Failure",
        "EngineError",
        "Owner",
        "Unloading",
        "Command",
        "Deadlines",
        "CPU_DEADLINES",
        "GPU_DEADLINES",
        "SAMPLE_RATE",
        "SHUTDOWN_GRACE",
        "LOAD_TIMEOUT",
        "lock",
        "not_loaded",
        "unexpected",
    ] {
        let found: Vec<_> = file
            .items
            .iter()
            .filter(|item| match item {
                Item::Struct(item) => item.ident == name,
                Item::Enum(item) => item.ident == name,
                Item::Const(item) => item.ident == name,
                Item::Fn(item) => item.sig.ident == name,
                _ => false,
            })
            .collect();
        assert_eq!(found.len(), 1, "missing/ambiguous source item {name}");
        let item = found[0];
        tokens.extend(quote! { #item });
    }
    for (owner, names) in [
        ("LoadSpec", &["pinned", "cpu_only", "cpu"][..]),
        ("Failure", &["worker_lost"]),
        ("Deadlines", &["for_device", "audio", "run"]),
        (
            "Shared",
            &[
                "epoch",
                "cancelled_since",
                "gpu_unavailable",
                "mark_gpu_unavailable",
            ],
        ),
        ("EngineSupervisor", &["unload"]),
        ("Unloading", &["wait"]),
        ("Worker", &["load"]),
        (
            "Owner",
            &[
                "run",
                "load",
                "unload",
                "effective_spec",
                "cpu_only_worker",
                "start_worker",
                "start",
                "publish_loaded",
                "ensure_worker",
                "worker_lost",
                "deadlines",
                "transcribe",
            ],
        ),
    ] {
        let methods: Vec<_> = names
            .iter()
            .map(|name| {
                let found: Vec<_> = file
                    .items
                    .iter()
                    .filter_map(|item| match item {
                        Item::Impl(item)
                            if item.trait_.is_none()
                                && matches!(item.self_ty.as_ref(),
                    Type::Path(path) if path.path.is_ident(owner)) =>
                        {
                            Some(item)
                        }
                        _ => None,
                    })
                    .flat_map(|item| &item.items)
                    .filter_map(|item| match item {
                        ImplItem::Fn(method) if method.sig.ident == *name => Some(method),
                        _ => None,
                    })
                    .collect();
                assert_eq!(found.len(), 1, "missing/ambiguous {owner}::{name}");
                found[0]
            })
            .collect();
        let owner = format_ident!("{owner}");
        tokens.extend(quote! { impl #owner { #(#methods)* } });
    }
    for (owner, trait_name) in [
        ("Worker", "Drop"),
        ("EngineError", "From"),
        ("Failure", "Display"),
        ("EngineError", "Display"),
    ] {
        let found: Vec<_> = file
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Impl(item)
                    if matches!(item.self_ty.as_ref(), Type::Path(path)
                if path.path.is_ident(owner))
                        && item.trait_.as_ref().is_some_and(|(_, path, _)| {
                            path.segments
                                .last()
                                .is_some_and(|part| part.ident == trait_name)
                        }) =>
                {
                    Some(item)
                }
                _ => None,
            })
            .collect();
        assert_eq!(found.len(), 1, "missing/ambiguous {trait_name} for {owner}");
        let item = found[0];
        tokens.extend(quote! { #item });
    }
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let extracted = syn::parse2(tokens).expect("parse extracted methods");
    fs::write(
        output.join("production.rs"),
        prettyplease::unparse(&extracted),
    )
    .unwrap();
}
