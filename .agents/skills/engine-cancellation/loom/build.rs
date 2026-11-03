use quote::{format_ident, quote};
use std::{env, fs, path::PathBuf};
use syn::{ImplItem, Item, Type};

fn main() {
    println!("cargo:rerun-if-env-changed=HANDY_FORMAL_SOURCE_ROOT");
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"));
    let root = env::var_os("HANDY_FORMAL_SOURCE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.ancestors().nth(4).expect("repository root").into());
    let source = root.join("src-tauri/src/engine_supervisor/supervisor.rs");
    println!("cargo:rerun-if-changed={}", source.display());
    let text = fs::read_to_string(&source).expect("read production supervisor.rs");
    let file = syn::parse_file(&text).expect("parse production supervisor.rs");
    let mut extracted = extract_methods(&file, "Shared", &["epoch", "cancelled_since"]);
    extracted.extend(extract_methods(&file, "EngineSupervisor", &["cancel"]));
    extracted.extend(extract_methods(&file, "Owner", &["call_work"]));

    let locks: Vec<_> = file
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Fn(function) if function.sig.ident == "lock" => Some(function),
            _ => None,
        })
        .collect();
    assert_eq!(
        locks.len(),
        1,
        "production lock helper missing or ambiguous"
    );
    let lock = locks[0];
    extracted.extend(quote! { #lock });

    for (field_name, expected) in [
        ("cancel_epoch", quote! { AtomicU64 }),
        ("in_flight", quote! { Mutex<Option<InFlight>> }),
    ] {
        let field = file
            .items
            .iter()
            .find_map(|item| match item {
                Item::Struct(item) if item.ident == "Shared" => item
                    .fields
                    .iter()
                    .find(|field| field.ident.as_ref().is_some_and(|name| name == field_name)),
                _ => None,
            })
            .expect("covered Shared field missing");
        let actual = &field.ty;
        let normalized = |text: String| text.split_whitespace().collect::<String>();
        assert_eq!(
            normalized(quote!(#actual).to_string()),
            normalized(expected.to_string()),
            "Shared::{field_name} changed type; review Loom substitutions"
        );
    }

    let in_flight = file
        .items
        .iter()
        .find_map(|item| match item {
            Item::Struct(item) if item.ident == "InFlight" => Some(item),
            _ => None,
        })
        .expect("production InFlight missing");
    extracted.extend(quote! { #in_flight });
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("build output"));
    fs::write(output.join("production.rs"), extracted.to_string())
        .expect("write extracted methods");
}

fn extract_methods(file: &syn::File, owner: &str, names: &[&str]) -> proc_macro2::TokenStream {
    let methods: Vec<_> = names
        .iter()
        .map(|name| {
            let found: Vec<_> = file
                .items
                .iter()
                .filter_map(|item| {
                    let Item::Impl(item) = item else {
                        return None;
                    };
                    let Type::Path(path) = item.self_ty.as_ref() else {
                        return None;
                    };
                    (item.trait_.is_none() && path.path.is_ident(owner)).then_some(item)
                })
                .flat_map(|item| item.items.iter())
                .filter_map(|item| match item {
                    ImplItem::Fn(method) if method.sig.ident == *name => Some(method),
                    _ => None,
                })
                .collect();
            assert_eq!(
                found.len(),
                1,
                "production {owner}::{name} missing or ambiguous"
            );
            found[0]
        })
        .collect();
    let owner = format_ident!("{owner}");
    quote! { impl #owner { #(#methods)* } }
}
