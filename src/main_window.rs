//! The class of the document `MainWindow.xaml`: the rcom main window — port and
//! baud selectors, open/close, the virtualized log view and the send bar.

use ferroui_base::input::InputElementImpl;
use ferroui_base::interactivity::InteractiveImpl;
use ferroui_base::layout::LayoutableImpl;
use ferroui_base::{
    ferro_class, ferro_class_info, ferro_impl_classes, instantiate, FerroObjectImpl, Ref,
    StyledElementImpl, VisualImpl,
};
use ferroui_controls::platform::PlatformManager;
use ferroui_controls::primitives::TemplatedControlImpl;
use ferroui_controls::{
    ContentControlImpl, ControlImpl, TopLevelImpl, Window, WindowBaseImpl, WindowImpl,
};

#[repr(C)]
pub struct MainWindow {
    base: Window,
}

ferro_class!(MainWindow: Window);
ferro_impl_classes!(
    MainWindow: FerroObjectImpl,
    StyledElementImpl,
    VisualImpl,
    LayoutableImpl,
    InteractiveImpl,
    InputElementImpl,
    ControlImpl,
    TemplatedControlImpl,
    ContentControlImpl,
    TopLevelImpl,
    WindowBaseImpl,
    WindowImpl
);
ferro_class_info!(MainWindow {
    new: MainWindow::new
});

impl MainWindow {
    pub fn construct() -> Self {
        Self {
            base: Window::construct(PlatformManager::create_window()),
        }
    }

    pub fn new() -> Ref<Self> {
        let this = instantiate(Self::construct());
        Self::initialize_component(&this);
        // The persisted look, before the controller attaches and before the
        // main loop runs (ticket #25 — the prototype's proven pattern): the
        // requested theme variant (dark by default) and the teal accent ramp,
        // both declared on the window — a theme-variant root, so the variant
        // becomes the actual one for the whole tree.
        crate::app::theme::apply_startup_look(&this);
        // The markup is populated: the controller can resolve the named controls.
        crate::app::controller::MainController::attach(&this);
        Self::start_smoke_exit(&this);
        this
    }

    /// The smoke run: with `FERROUI_SMOKE_EXIT_MS=<n>` the main window closes
    /// itself after `n` milliseconds, ending the main loop (the canonical app-side
    /// pattern of ferroui-desktop's `hello_window` example).
    fn start_smoke_exit(this: &Ref<Self>) {
        let Ok(ms) = std::env::var("FERROUI_SMOKE_EXIT_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(|ms| ms.max(1))
            .ok_or(())
        else {
            return;
        };
        tracing::info!(smoke_exit_ms = ms, "smoke run: will close the main window");
        let window = this.clone();
        // The disposable keeps the timer alive; this one-shot lives as long as the
        // app, so it is leaked deliberately.
        std::mem::forget(ferroui_base::threading::DispatcherTimer::run_once(
            move || {
                tracing::info!("smoke run: closing the main window");
                window.close();
            },
            std::time::Duration::from_millis(ms),
            ferroui_base::threading::DispatcherPriority::NORMAL,
        ));
    }

    /// `InitializeComponent()`: populates the window from its document.
    fn initialize_component(this: &Ref<Self>) {
        if let Err(error) = crate::compiled_main_window::populate(None, this) {
            panic!("/MainWindow.xaml: {error}");
        }
    }
}
