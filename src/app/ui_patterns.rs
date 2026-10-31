//! The shared page-pattern builders (design-ui §4.1): the pieces every
//! bottom-pane page composes — the accent primary action, the destructive-only
//! confirm flyout (`confirm_flyout`), and the window-notification manager the
//! error matrix's rows 4–6 surface through.

use std::rc::Rc;

use ferroui_base::layout::{Orientation, VerticalAlignment};
use ferroui_base::media::{FontWeight, SolidColorBrush, TextAlignment, TextWrapping};
use ferroui_base::threading::DispatcherTimer;
use ferroui_base::reactive::IDisposable;
use ferroui_base::{CornerRadius, Ref, Thickness};
use ferroui_controls::notifications::{Notification, NotificationType, WindowNotificationManager};
use ferroui_controls::primitives::HeaderedContentControl;
use ferroui_controls::{Border, Button, Control, Flyout, ProgressBar, StackPanel, TabItem, TextBlock, TopLevel};

use rust_i18n::t;

/// Marks `button` as the page's one accent primary action (the theme's
/// `.accent` class styles it with `AccentFillColorDefaultBrush`).
pub fn accent(button: &Button) {
    button.classes().add("accent");
}

/// Sets a TabItem's header text (the XAML carries none — the i18n
/// dictionaries are the source).
pub fn tab_header(tab: &TabItem, text: &str) {
    tab.set_value(
        HeaderedContentControl::header_property(),
        Some(Rc::new(text.to_string())),
    );
}

/// The destructive-only confirm (§4.1): one line — *"… This cannot be
/// undone."* — with [Verb] (accent, verb restated) and [Cancel], anchored to
/// `owner` as a button-anchored `Flyout` (no modal exists at the pin; this is
/// the mechanism of record). `on_confirm` runs when the verb is clicked; the
/// flyout closes itself either way.
pub fn confirm_flyout(
    owner: &Button,
    label: String,
    verb: &'static str,
    on_confirm: impl Fn() + 'static,
) {
    let flyout = Flyout::new();

    let panel = StackPanel::new();
    panel.set_orientation(Orientation::Vertical);
    panel.set_spacing(12.0);

    let text = TextBlock::new();
    text.set_text(Some(&label));
    text.set_max_width(280.0);
    text.set_text_wrapping(TextWrapping::Wrap);
    panel.children().add(text.upcast::<Control>());

    let row = StackPanel::new();
    row.set_orientation(Orientation::Horizontal);
    row.set_spacing(8.0);
    row.set_horizontal_alignment(ferroui_base::layout::HorizontalAlignment::Right);

    let cancel = Button::new();
    cancel.set_content(Some(Rc::new(t!("cancel").to_string())));
    let flyout_for_cancel = flyout.clone();
    cancel.add_handler(Button::click_event(), move |_, _| {
        flyout_for_cancel.set_is_open(false);
    });
    row.children().add(cancel.upcast::<Control>());

    let confirm = Button::new();
    confirm.set_content(Some(Rc::new(t!(verb).to_string())));
    accent(&confirm);
    let flyout_for_confirm = flyout.clone();
    confirm.add_handler(Button::click_event(), move |_, _| {
        flyout_for_confirm.set_is_open(false);
        on_confirm();
    });
    row.children().add(confirm.upcast::<Control>());

    panel.children().add(row.upcast::<Control>());
    flyout.set_content(Some(Control::boxed(panel)));
    owner.set_flyout(&flyout);
}

/// The app-mounted window-notification manager (§4.1 rows 4–6 only, never for
/// anything the user just clicked). Created once against the window's
/// top-level; the theme styles it.
pub struct Notifier {
    manager: Ref<WindowNotificationManager>,
}

impl Notifier {
    pub fn mounted(host: &Ref<TopLevel>) -> Self {
        Self { manager: WindowNotificationManager::with_host(Some(host)) }
    }

    /// Shows an information notification (the hotplug toasts).
    pub fn inform(&self, title: &str, message: &str) {
        self.show(NotificationType::Information, title, message);
    }

    /// Shows a warning notification (page-scoped async errors while the page
    /// is not selected).
    pub fn warn(&self, title: &str, message: &str) {
        self.show(NotificationType::Warning, title, message);
    }

    fn show(&self, type_: NotificationType, title: &str, message: &str) {
        let notification = Notification::new(
            Some(title.to_string()),
            Some(message.to_string()),
            type_,
            None,
            None,
            None,
        );
        self.manager.show(notification);
    }
}

/// The busy rule's one sanctioned indeterminate ProgressBar slot: the session
/// toolbar while the port-open future runs > ~500 ms. `show_after` arms a
/// one-shot timer — no spinner for fast opens, the bar appears only when the
/// future runs long. The returned guard disarms on drop.
pub struct OpenProgressGuard {
    /// The one-shot reveal timer (kept alive; dropped = stopped).
    _timer: Rc<dyn IDisposable>,
    bar: Ref<ProgressBar>,
}

impl OpenProgressGuard {
    /// Arms the >500 ms reveal for `progress` (which this call hides first).
    pub fn arm(progress: &Ref<ProgressBar>) -> Self {
        progress.set_is_indeterminate(true);
        progress.set_is_visible(false);
        let bar = progress.clone();
        let timer = DispatcherTimer::run_once(
            move || {
                bar.set_is_visible(true);
            },
            std::time::Duration::from_millis(500),
            ferroui_base::threading::DispatcherPriority::BACKGROUND,
        );
        Self { _timer: timer, bar: progress.clone() }
    }
}

impl Drop for OpenProgressGuard {
    fn drop(&mut self) {
        // A completed open stops the reveal; the bar hides again either way.
        self.bar.set_is_visible(false);
    }
}

/// The dim empty-state caption of a page (§4.1): one line naming the next
/// action, centered in the content area. Returns the TextBlock for the caller
/// to place; `set_empty_caption` toggles it.
pub fn empty_caption() -> Ref<TextBlock> {
    let caption = TextBlock::new();
    caption.set_font_size(12.0);
    caption.set_opacity(0.6);
    caption.set_text_alignment(TextAlignment::Center);
    caption.set_horizontal_alignment(ferroui_base::layout::HorizontalAlignment::Center);
    caption.set_vertical_alignment(VerticalAlignment::Center);
    caption.set_text_wrapping(TextWrapping::Wrap);
    caption.set_margin(Thickness::new(16.0, 8.0, 16.0, 8.0));
    caption
}

/// A translucent status chip (the session state chip): rounded, padded, with
/// the label TextBlock inside.
pub fn chip(background: ferroui_base::media::Color, foreground: ferroui_base::media::Color) -> (Ref<Border>, Ref<TextBlock>) {
    let border = Border::new();
    border.set_corner_radius(CornerRadius::uniform(3.0));
    border.set_padding(Thickness::new(8.0, 1.0, 8.0, 1.0));
    border.set_vertical_alignment(VerticalAlignment::Center);
    border.set_value(
        Border::background_property(),
        Some(SolidColorBrush::with_color(background).into()),
    );
    let text = TextBlock::new();
    text.set_font_size(11.0);
    text.set_font_weight(FontWeight::SemiBold);
    text.set_value(
        TextBlock::foreground_property(),
        Some(ferroui_base::media::SolidColorBrush::with_color(foreground).into()),
    );
    let child = text.clone();
    border.set_child(Some(child.upcast::<Control>()));
    (border, text)
}
