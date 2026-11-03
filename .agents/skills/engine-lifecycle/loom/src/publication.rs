use loom::sync::atomic::{AtomicBool, Ordering};
use loom::sync::{Arc, Mutex};

struct ModelInfo {
    name: String,
}
struct ModelStateEvent {
    event_type: String,
    model_id: Option<String>,
    model_name: Option<String>,
    error: Option<String>,
}
struct App {
    events: Mutex<Vec<ModelStateEvent>>,
}
impl App {
    fn emit(&self, _name: &str, event: ModelStateEvent) -> Result<(), ()> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }
}
struct Unloading;
struct Engine {
    pending: AtomicBool,
    loaded: AtomicBool,
}
impl Engine {
    fn unload(&self) -> Unloading {
        self.pending.store(true, Ordering::SeqCst);
        self.loaded.store(false, Ordering::SeqCst);
        Unloading
    }
}
struct LanguageDetector;
impl LanguageDetector {
    fn unload(&self) {}
}
struct TranscriptionManager {
    engine: Engine,
    language_detector: LanguageDetector,
    current_model_id: Mutex<Option<String>>,
    app_handle: App,
}
impl TranscriptionManager {
    fn touch_activity(&self) {}
}

include!(concat!(env!("OUT_DIR"), "/publication.rs"));

fn setup() -> Arc<TranscriptionManager> {
    Arc::new(TranscriptionManager {
        engine: Engine {
            pending: AtomicBool::new(false),
            loaded: AtomicBool::new(true),
        },
        language_detector: LanguageDetector,
        current_model_id: Mutex::new(None),
        app_handle: App {
            events: Mutex::new(Vec::new()),
        },
    })
}

fn race(check_ui: bool) {
    super::builder().check(move || {
        let manager = setup();
        let loading = Arc::clone(&manager);
        let load = loom::thread::spawn(move || {
            loading.publish_completed_load(
                "fixture",
                &ModelInfo {
                    name: "fixture".into(),
                },
            )
        });
        let unloading = Arc::clone(&manager);
        let unload = loom::thread::spawn(move || {
            unloading.begin_unload();
        });
        load.join().unwrap();
        unload.join().unwrap();
        assert!(manager.engine.pending.load(Ordering::SeqCst));
        assert!(!manager.engine.loaded.load(Ordering::SeqCst));
        if check_ui {
            let loaded_event = manager
                .app_handle
                .events
                .lock()
                .unwrap()
                .last()
                .is_some_and(|event| event.event_type == "loading_completed");
            assert!(
                !loaded_event,
                "late manager completion restored loaded UI during pending unload"
            );
        } else {
            let resurrected = manager.current_model_id.lock().unwrap().is_some();
            assert!(
                !resurrected,
                "late manager completion restored model id during pending unload"
            );
        }
    });
}

#[test]
fn manager_loaded_id_respects_pending_unload() {
    race(false);
}

#[test]
fn manager_loaded_ui_respects_pending_unload() {
    race(true);
}

#[test]
fn manager_unload_clears_published_state() {
    super::builder().check(|| {
        let manager = setup();
        manager.publish_completed_load(
            "fixture",
            &ModelInfo {
                name: "fixture".into(),
            },
        );
        assert!(manager.current_model_id.lock().unwrap().is_some());
        manager.begin_unload();
        let cleared = manager.current_model_id.lock().unwrap().is_none();
        let unloaded_event = manager
            .app_handle
            .events
            .lock()
            .unwrap()
            .last()
            .is_some_and(|event| event.event_type == "unloaded");
        assert!(cleared, "manager unload retained published model id");
        assert!(unloaded_event, "manager unload omitted unloaded UI event");
    });
}
