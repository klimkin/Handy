use quote::{format_ident, quote};
use std::{env, fs, path::PathBuf};
use syn::{ImplItem, Item, Type};

fn methods(file: &syn::File, owner: &str, names: &[&str]) -> proc_macro2::TokenStream {
    let methods: Vec<_> = names
        .iter()
        .map(|name| {
            let found: Vec<_> = file
                .items
                .iter()
                .filter_map(|item| {
                    let Item::Impl(item) = item else { return None };
                    let Type::Path(path) = item.self_ty.as_ref() else {
                        return None;
                    };
                    (path.path.segments.len() == 1 && path.path.segments[0].ident == owner)
                        .then_some(item)
                })
                .flat_map(|item| item.items.iter())
                .filter_map(|item| match item {
                    ImplItem::Fn(method) if method.sig.ident == *name => Some(method),
                    _ => None,
                })
                .collect();
            assert_eq!(found.len(), 1, "{owner}::{name} missing or ambiguous");
            found[0]
        })
        .collect();
    let owner = format_ident!("{owner}");
    quote! { impl #owner { #(#methods)* } }
}

fn named(file: &syn::File, name: &str) -> proc_macro2::TokenStream {
    let found: Vec<_> = file
        .items
        .iter()
        .filter(|item| match item {
            Item::Struct(item) => item.ident == name,
            Item::Enum(item) => item.ident == name,
            _ => false,
        })
        .collect();
    assert_eq!(found.len(), 1, "{name} missing or ambiguous");
    let item = found[0];
    quote! { #item }
}

fn main() {
    println!("cargo:rerun-if-env-changed=HANDY_FORMAL_SOURCE_ROOT");
    println!("cargo:rerun-if-env-changed=HANDY_RECORDER_SOURCE");
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"));
    let root = env::var_os("HANDY_FORMAL_SOURCE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.ancestors().nth(4).expect("repository root").into());
    let read = |relative: &str| {
        let path = root.join(relative);
        println!("cargo:rerun-if-changed={}", path.display());
        syn::parse_file(&fs::read_to_string(path).expect("read production source"))
            .expect("parse production source")
    };
    let manager = read("src-tauri/src/managers/transcription.rs");
    let engine = read("src-tauri/src/engine_supervisor/supervisor.rs");
    let audio = read("src-tauri/src/managers/audio.rs");
    let sink = read("src-tauri/src/audio_toolkit/audio/sink.rs");
    let actions = read("src-tauri/src/actions.rs");
    let recorder = PathBuf::from(
        env::var_os("HANDY_RECORDER_SOURCE")
            .expect("check.py supplies handy-recorder 0.1.0 source directory"),
    );
    let recorder_engine = recorder.join("src/capture/engine.rs");
    println!("cargo:rerun-if-changed={}", recorder_engine.display());
    let recorder_engine = syn::parse_file(
        &fs::read_to_string(recorder_engine).expect("read handy-recorder engine.rs"),
    )
    .expect("parse handy-recorder engine.rs");
    lifecycle_boundary(&audio, &actions, &recorder_engine);
    let mut manager_code = named(&manager, "StreamWorkerGuard");
    let drops: Vec<_> = manager
        .items
        .iter()
        .filter_map(|item| {
            let Item::Impl(item) = item else { return None };
            let Type::Path(path) = item.self_ty.as_ref() else {
                return None;
            };
            (path.path.is_ident("StreamWorkerGuard") && item.trait_.is_some()).then_some(item)
        })
        .collect();
    assert_eq!(
        drops.len(),
        1,
        "StreamWorkerGuard Drop missing or ambiguous"
    );
    let drop = drops[0];
    manager_code.extend(quote! { #drop });
    for name in ["StreamRouter", "Route", "StreamCmd"] {
        manager_code.extend(named(&manager, name));
    }
    manager_code.extend(methods(
        &manager,
        "StreamRouter",
        &["new", "open", "take", "clear", "feed", "is_open"],
    ));
    manager_code.extend(methods(&sink, "CaptureSink", &["emit"]));
    manager_code.extend(methods(
        &manager,
        "TranscriptionManager",
        &["start_stream", "finalize_route"],
    ));
    let mut engine_code = methods(&engine, "Shared", &["epoch", "cancelled_since"]);
    engine_code.extend(methods(
        &engine,
        "Owner",
        &["feed", "finalize", "end_stream"],
    ));
    engine_code.extend(named(&engine, "StreamHandle"));
    engine_code.extend(methods(&engine, "StreamHandle", &["feed", "finalize"]));
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("build output"));
    fs::write(output.join("manager.rs"), manager_code.to_string())
        .expect("write manager extraction");
    fs::write(output.join("engine.rs"), engine_code.to_string()).expect("write engine extraction");
}

fn compact(tokens: proc_macro2::TokenStream) -> String {
    tokens.to_string().split_whitespace().collect()
}

fn ordered(body: &str, operations: &[&str]) {
    let mut remaining = body;
    for operation in operations {
        let offset = remaining.find(operation).unwrap_or_else(|| {
            panic!("recorder overlap assumption drifted at {operation}; review lifecycle")
        });
        remaining = &remaining[offset + operation.len()..];
    }
}

fn lifecycle_boundary(audio: &syn::File, actions: &syn::File, recorder: &syn::File) {
    let start_action = compact(methods(actions, "TranscribeAction", &["start"]));
    ordered(
        &start_action,
        &["tm.start_stream(run)", "rm.try_start_recording("],
    );
    let constructor = compact(methods(audio, "AudioRecordingManager", &["new"]));
    assert!(
        constructor.contains(
            "let on_audio:AudioFrameCallback=Arc::new(move|frame|stream_router.feed(frame));"
                .replace(' ', "")
                .as_str()
        ),
        "audio callback no longer captures shared router; review stale-frame witness"
    );
    let collect_audio = compact(methods(
        audio,
        "AudioRecordingManager",
        &["collect_recording"],
    ));
    ordered(
        &collect_audio,
        &[
            "microphone.recorder.stop()",
            "Err(e)=>{",
            "microphone.failed.store(true,Ordering::Release)",
        ],
    );
    let stop_audio = compact(methods(audio, "AudioRecordingManager", &["stop_recording"]));
    ordered(
        &stop_audio,
        &[
            "self.collect_recording(microphone)",
            "*self.is_recording.lock().unwrap()=false",
            "RecordingState::Idle",
        ],
    );
    let reopen = compact(methods(
        audio,
        "AudioRecordingManager",
        &["start_microphone_stream"],
    ));
    ordered(
        &reopen,
        &[
            "OpenMicrophone::needs_reopen",
            "self.close_microphone(microphone)",
            "open_microphone_on_channel(",
        ],
    );
    let close = compact(methods(
        audio,
        "AudioRecordingManager",
        &["close_microphone"],
    ));
    ordered(&close, &["microphone.recorder.close()", "warn!"]);

    let stop = compact(methods(recorder, "Engine", &["stop"]));
    ordered(
        &stop,
        &[
            "let result=self.collect();".replace(' ', "").as_str(),
            "*self.slot.lock().unwrap()=Slot::Idle",
            "result",
        ],
    );
    let collect = compact(methods(recorder, "Engine", &["collect"]));
    ordered(
        &collect,
        &[
            "reply_rx.recv_timeout(self.timeouts.stop)",
            "Err(mpsc::RecvTimeoutError::Timeout)=>{",
            "ErrorKind::SinkStalled",
            "Err(failure)",
        ],
    );
    assert!(
        !collect.contains(".join("),
        "recorder collect now waits for delivery; review overlap witness"
    );
    let shutdown = compact(methods(recorder, "Engine", &["shutdown"]));
    ordered(
        &shutdown,
        &[
            ".delivery_exit.recv_timeout(self.timeouts.delivery_exit)",
            "threads.delivery.join()",
            "Err(mpsc::RecvTimeoutError::Timeout)=>{",
            "itisdetached",
        ],
    );
    println!("cargo:warning=stale-frame boundary checked against audio manager and handy-recorder 0.1.0 stop timeout/detach source");
}
