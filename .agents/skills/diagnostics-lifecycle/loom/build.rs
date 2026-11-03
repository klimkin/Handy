use quote::quote;
use std::{env, fs, path::PathBuf};
use syn::{ImplItem, Item, Type};

fn main() {
    println!("cargo:rerun-if-env-changed=HANDY_FORMAL_SOURCE_ROOT");
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let root = env::var_os("HANDY_FORMAL_SOURCE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.ancestors().nth(4).unwrap().into());
    let source = root.join("src-tauri/src/diagnostics.rs");
    println!("cargo:rerun-if-changed={}", source.display());
    let file = syn::parse_file(&fs::read_to_string(source).unwrap()).unwrap();
    let mut real = proc_macro2::TokenStream::new();
    let mut race = proc_macro2::TokenStream::new();
    let shapes = [
        "Capture",
        "Event",
        "Recorder",
        "Status",
        "DiagError",
        "RunId",
        "Phase",
        "ThreadTag",
        "Args",
        "Name",
        "Code",
    ];
    let methods = ["new", "emit", "stop"];
    let mut found = Vec::new();
    for item in &file.items {
        let excluded = match item {
            Item::Mod(item) => item.ident == "tests",
            Item::Const(item) => item.ident == "NAME_LIST" || item.ident == "CODE_LIST",
            _ => false,
        };
        if !excluded {
            real.extend(quote! { #item });
        }
        match item {
            Item::Struct(item) if shapes.iter().any(|name| item.ident == *name) => {
                race.extend(quote! { #item });
            }
            Item::Enum(item) if shapes.iter().any(|name| item.ident == *name) => {
                race.extend(quote! { #item });
            }
            Item::Const(item) if item.ident == "STOP_TIMEOUT" => {
                race.extend(quote! { #item });
            }
            Item::Impl(item) => {
                let Type::Path(path) = item.self_ty.as_ref() else {
                    continue;
                };
                if item.trait_.is_none() && path.path.is_ident("Recorder") {
                    for method in &item.items {
                        if let ImplItem::Fn(method) = method {
                            if methods.iter().any(|name| method.sig.ident == *name) {
                                found.push(method.sig.ident.to_string());
                                race.extend(quote! { impl Recorder { #method } });
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    for method in methods {
        assert_eq!(
            found.iter().filter(|name| *name == method).count(),
            1,
            "missing/ambiguous Recorder::{method}"
        );
    }
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    fs::write(output.join("real.rs"), real.to_string()).unwrap();
    fs::write(output.join("race.rs"), race.to_string()).unwrap();
}
