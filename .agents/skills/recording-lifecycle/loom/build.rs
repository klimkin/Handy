use quote::{format_ident, quote, ToTokens};
use std::{env, fs, path::PathBuf};
use syn::{visit::Visit, Expr, ImplItem, Item, Stmt, Type};

struct MuteAdapter;
impl syn::visit_mut::VisitMut for MuteAdapter {
    fn visit_expr_mut(&mut self, expression: &mut Expr) {
        if let Expr::Call(call) = expression {
            if matches!(call.func.as_ref(), Expr::Path(path) if path.path.is_ident("set_mute")) {
                let arguments = &call.args;
                *expression = syn::parse_quote! { self.app_handle.set_mute(#arguments) };
                return;
            }
        }
        syn::visit_mut::visit_expr_mut(self, expression);
    }
}

fn read(root: &std::path::Path, path: &str) -> syn::File {
    let source = root.join(path);
    println!("cargo:rerun-if-changed={}", source.display());
    syn::parse_file(&fs::read_to_string(source).expect("read production Rust"))
        .expect("parse production Rust")
}

fn methods(file: &syn::File, owner: &str, names: &[&str]) -> proc_macro2::TokenStream {
    let mut found = Vec::new();
    for name in names {
        let matches: Vec<_> = file
            .items
            .iter()
            .filter_map(|item| {
                let Item::Impl(item) = item else { return None };
                let Type::Path(path) = item.self_ty.as_ref() else {
                    return None;
                };
                (item.trait_.is_none() && path.path.is_ident(owner)).then_some(item)
            })
            .flat_map(|item| &item.items)
            .filter_map(|item| match item {
                ImplItem::Fn(item) if item.sig.ident == *name => Some(item),
                _ => None,
            })
            .collect();
        assert_eq!(matches.len(), 1, "{owner}::{name} missing or ambiguous");
        found.push(matches[0]);
    }
    let owner = format_ident!("{owner}");
    quote! { impl #owner { #(#found)* } }
}

#[derive(Default)]
struct Closures {
    ready: Vec<syn::Block>,
    paste: Vec<syn::Block>,
}

impl<'ast> Visit<'ast> for Closures {
    fn visit_expr_closure(&mut self, closure: &'ast syn::ExprClosure) {
        if let Expr::Block(body) = closure.body.as_ref() {
            let text = body.to_token_stream().to_string();
            if text.contains("rm_clone . is_recording_readiness_current") {
                self.ready.push(body.block.clone());
            }
            if text.contains("rm_for_paste . was_cancelled_since") {
                self.paste.push(body.block.clone());
            }
        }
        syn::visit::visit_expr_closure(self, closure);
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=HANDY_FORMAL_SOURCE_ROOT");
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let root = env::var_os("HANDY_FORMAL_SOURCE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.ancestors().nth(4).unwrap().into());
    let coordinator = read(&root, "src-tauri/src/transcription_coordinator.rs");
    let mut audio = read(&root, "src-tauri/src/managers/audio.rs");
    let actions = read(&root, "src-tauri/src/actions.rs");
    let settings = read(&root, "src-tauri/src/settings.rs");
    let mut output = proc_macro2::TokenStream::new();
    let items = [
        "DEBOUNCE",
        "RELEASE_GRACE",
        "PttAction",
        "PendingRelease",
        "PendingPress",
        "Remembered",
        "Hold",
        "BusyAction",
        "classify_busy_input",
        "Stage",
        "InputEvent",
        "Effect",
        "classify_ptt_event",
        "CoordinatorState",
        "run_effect",
    ];
    for name in items {
        let matches: Vec<_> = coordinator
            .items
            .iter()
            .filter(|item| match item {
                Item::Const(item) => item.ident == name,
                Item::Enum(item) => item.ident == name,
                Item::Struct(item) => item.ident == name,
                Item::Fn(item) => item.sig.ident == name,
                _ => false,
            })
            .collect();
        assert_eq!(matches.len(), 1, "coordinator {name} missing or ambiguous");
        matches[0].to_tokens(&mut output);
    }
    for owner in ["CoordinatorState", "PendingPress", "InputEvent"] {
        let matches: Vec<_> = coordinator
            .items
            .iter()
            .filter(|item| match item {
                Item::Impl(item) => {
                    matches!(item.self_ty.as_ref(), Type::Path(path) if path.path.is_ident(owner))
                }
                _ => false,
            })
            .collect();
        assert_eq!(matches.len(), 1, "{owner} impl missing or ambiguous");
        matches[0].to_tokens(&mut output);
    }
    let activation = settings
        .items
        .iter()
        .find_map(|item| match item {
            Item::Enum(item) if item.ident == "ShortcutActivation" => Some(item),
            _ => None,
        })
        .expect("ShortcutActivation missing");
    let variants = &activation.variants;
    output.extend(quote! { #[derive(Clone, Copy, Default, PartialEq, Eq)] enum ShortcutActivation { #variants } });
    // Adapter changes only the external OS mute call's receiver, allowing its
    // stub to record the app/session without thread-local Loom-owned values.
    syn::visit_mut::VisitMut::visit_file_mut(&mut MuteAdapter, &mut audio);
    for (name, expected) in [
        ("capture_generation", quote! { Arc<AtomicU64> }),
        ("cancel_generation", quote! { Arc<AtomicU64> }),
        ("is_open", quote! { Arc<Mutex<bool>> }),
        ("mute_state", quote! { Arc<Mutex<MuteState>> }),
    ] {
        let field = audio
            .items
            .iter()
            .find_map(|item| match item {
                Item::Struct(item) if item.ident == "AudioRecordingManager" => item
                    .fields
                    .iter()
                    .find(|field| field.ident.as_ref().is_some_and(|ident| ident == name)),
                _ => None,
            })
            .expect("covered audio field missing");
        let actual = &field.ty;
        let normalize = |tokens: proc_macro2::TokenStream| {
            tokens.to_string().split_whitespace().collect::<String>()
        };
        assert_eq!(
            normalize(quote!(#actual)),
            normalize(expected),
            "audio {name} type changed"
        );
    }
    let mute_state = audio
        .items
        .iter()
        .find_map(|item| match item {
            Item::Struct(item) if item.ident == "MuteState" => Some(item),
            _ => None,
        })
        .expect("MuteState missing");
    output.extend(quote! { #mute_state });
    output.extend(methods(
        &audio,
        "AudioRecordingManager",
        &[
            "invalidate_recording_readiness",
            "is_recording_readiness_current",
            "was_cancelled_since",
            "apply_mute",
        ],
    ));

    let mut closures = Closures::default();
    closures.visit_file(&actions);
    assert_eq!(closures.ready.len(), 1, "ready closure changed");
    assert_eq!(closures.paste.len(), 1, "paste closure changed");
    let ready = &closures.ready[0].stmts;
    // Entry boundary is after first-sample wait and diagnostics/preview delay.
    let first = ready
        .iter()
        .position(|stmt| {
            stmt.to_token_stream()
                .to_string()
                .starts_with("if ! rm_clone . is_recording_readiness_current")
        })
        .expect("first readiness guard changed");
    let ready = &ready[first..];
    output.extend(quote! {
        fn production_ready(rm_clone: Arc<AudioRecordingManager>, generation: u64, app_clone: App) {
            #(#ready)*
        }
    });
    let paste = &closures.paste[0].stmts;
    let first_guard = paste.first().expect("paste guard missing");
    assert!(
        first_guard
            .to_token_stream()
            .to_string()
            .contains("rm_for_paste . was_cancelled_since"),
        "paste entry guard changed"
    );
    let boundary = paste
        .iter()
        .position(|stmt| {
            if let Stmt::Local(local) = stmt {
                if let Some(init) = &local.init {
                    if let Expr::Match(expr) = init.expr.as_ref() {
                        return expr.expr.to_token_stream().to_string()
                            == "utils :: paste (final_text , ah_clone . clone ())";
                    }
                }
            }
            false
        })
        .expect("paste commitment call changed");
    let prefix = &paste[..boundary];
    output.extend(quote! {
        fn production_paste(rm_for_paste: Arc<AudioRecordingManager>, cancel_generation: u64,
                            ah_clone: App, session: u64, final_text: String) {
            let run = ();
            #(#prefix)*
            utils::paste(final_text, ah_clone.clone());
        }
    });
    let destination = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("production.rs");
    let file = syn::parse2(output).expect("parse extracted production code");
    fs::write(destination, prettyplease::unparse(&file)).expect("write extracted production code");
}
