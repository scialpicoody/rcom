//! The controller of `MainWindow`: looks the named controls up in the window's name
//! scope, wires their events to core commands, and pumps the core's messages into
//! the log view. Lives on the UI thread only (`Rc` state; every field is accessed
//! from handlers and the dispatcher-timer pump, which run there).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use ferroui_base::controls::NameScopeExtensions;
use ferroui_base::input::Key;
use ferroui_base::threading::{DispatcherPriority, DispatcherTimer};
use ferroui_base::{BoxedValue, Ref, StyledElement};
use ferroui_controls::{Button, ComboBox, ListBox, TextBox};

use super::host::{core_host, Command, UiMsg};
use rcom::core::bus::PortState;
use rcom::core::port::PortInfo;



/// The baud rates the baud selector offers; 115200 is preselected.
const BAUD_RATES: [u32; 7] = [9_600, 19_200, 38_400, 57_600, 115_200, 230_400, 460_800];

/// How many rows the log view keeps before it drops the oldest: the virtualized
/// list keeps memory sane, llcom-style.
const ROW_CAP: usize = 5000;

/// How often the UI pumps messages from the core driver.
const PUMP_INTERVAL: Duration = Duration::from_millis(30);

pub struct MainController {
    port_selector: Ref<ComboBox>,
    baud_selector: Ref<ComboBox>,
    open_close: Ref<Button>,
    refresh_button: Ref<Button>,
    send_box: Ref<TextBox>,
    send_button: Ref<Button>,
    log: Ref<ListBox>,
    ports: RefCell<Vec<PortInfo>>,
    is_open: Cell<bool>,
    row_count: Cell<usize>,
}

impl MainController {
    /// Builds the controller for `window`: resolves the named controls of
    /// `MainWindow.xaml`, subscribes their events and starts the pump. Call on the
    /// UI thread, after the window's markup is populated.
    pub fn attach(window: &Ref<crate::MainWindow>) {
        let element: &Ref<StyledElement> = window.upcast_ref();

        let this = Rc::new(Self {
            port_selector: NameScopeExtensions::get(element, "PortSelector"),
            baud_selector: NameScopeExtensions::get(element, "BaudSelector"),
            open_close: NameScopeExtensions::get(element, "OpenCloseButton"),
            refresh_button: NameScopeExtensions::get(element, "RefreshButton"),
            send_box: NameScopeExtensions::get(element, "SendBox"),
            send_button: NameScopeExtensions::get(element, "SendButton"),
            log: NameScopeExtensions::get(element, "LogList"),
            ports: RefCell::new(Vec::new()),
            is_open: Cell::new(false),
            row_count: Cell::new(0),
        });

        this.fill_baud_selector();
        wire_events(&this);
        start_pump(&this);

        // Kick off the first port listing.
        core_host().commands.send(Command::RefreshPorts).ok();
    }

    fn fill_baud_selector(&self) {
        let items = self.baud_selector.items();
        for rate in BAUD_RATES {
            let value: BoxedValue = Rc::new(rate.to_string());
            items.add(Some(value));
        }
        let default = BAUD_RATES.iter().position(|rate| *rate == 115_200).unwrap_or(0);
        self.baud_selector.set_selected_index(default as i32);
    }

    /// The selected baud rate, defaulting to 115200 while nothing is selected.
    fn selected_baud(&self) -> u32 {
        let index = self.baud_selector.selected_index();
        if index >= 0 {
            BAUD_RATES.get(index as usize).copied().unwrap_or(115_200)
        } else {
            115_200
        }
    }

    /// Open/Close button: opens the selected port at the selected baud, or closes
    /// the current session.
    fn toggle_open_close(&self) {
        if self.is_open.get() {
            core_host().commands.send(Command::Close).ok();
            return;
        }
        let index = self.port_selector.selected_index();
        let port = self
            .ports
            .borrow()
            .get(index.max(0) as usize)
            .map(|info| info.name.clone());
        let Some(port) = port else {
            self.append_row("## select a port first".into());
            return;
        };
        core_host().commands
            .send(Command::Open { name: port, baud: self.selected_baud() })
            .ok();
    }

    fn refresh_ports(&self) {
        core_host().commands.send(Command::RefreshPorts).ok();
    }

    /// Send button / Enter in the send box: the box's text as UTF-8 bytes.
    fn send_box_text(&self) {
        let Some(text) = self.send_box.text() else { return };
        if text.is_empty() {
            return;
        }
        if core_host().commands.send(Command::SendText(text)).is_ok() {
            self.send_box.set_text(None);
        }
    }

    fn set_open_state(&self, state: PortState) {
        self.is_open.set(state == PortState::Open);
        let label = if self.is_open.get() { "Close" } else { "Open" };
        self.open_close.set_content(Some(Rc::new(label.to_string())));
    }

    fn append_row(&self, row: String) {
        let items = self.log.items();
        let value: BoxedValue = Rc::new(row);
        items.add(Some(value));
        let count = self.row_count.get() + 1;
        if count > ROW_CAP {
            items.remove_at(0);
            self.row_count.set(count - 1);
        } else {
            self.row_count.set(count);
        }
        // Auto-scroll to the newest row (virtualization makes this cheap).
        self.log.scroll_into_view(self.log.item_count() - 1);
    }

    fn apply(&self, message: UiMsg) {
        match message {
            UiMsg::Row(row) => self.append_row(row),
            UiMsg::Ports(ports) => self.show_ports(ports),
            UiMsg::PortState(state) => self.set_open_state(state),
        }
    }

    fn show_ports(&self, ports: Vec<PortInfo>) {
        let remember = self.port_selector.selected_index();
        let count = ports.len() as i32;
        let items = self.port_selector.items();
        items.clear();
        for info in &ports {
            let name = match (&info.description, info.is_mock) {
                (Some(description), false) => format!("{}  ({description})", info.name),
                _ => info.name.clone(),
            };
            let value: BoxedValue = Rc::new(name);
            items.add(Some(value));
        }
        *self.ports.borrow_mut() = ports;
        // Keep the previous selection when it still exists, else the first port.
        let kept = if remember >= 0 && remember < count {
            remember
        } else if count > 0 {
            0
        } else {
            -1
        };
        self.port_selector.set_selected_index(kept);
    }
}

/// Subscribes the control events of the window (runs on the UI thread; the handlers
/// keep the controller alive for the window's lifetime).
fn wire_events(this: &Rc<MainController>) {
    this.open_close.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| this.toggle_open_close()
    });

    this.refresh_button.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| this.refresh_ports()
    });

    this.send_button.add_handler(Button::click_event(), {
        let this = Rc::clone(this);
        move |_, _| this.send_box_text()
    });

    // Enter in the send box sends.
    this.send_box.key_down({
        let this = Rc::clone(this);
        move |_, args| {
            if args.key == Key::Enter {
                args.set_handled(true);
                this.send_box_text();
            }
        }
    });
}

ferroui_base::ferro_thread_local! {
    /// The UI-thread handle the pump uses to reach the controller, and the timer
    /// that keeps the pump running (dropping a timer stops it).
    static PUMP: RefCell<Pump> = RefCell::new(Pump { controller: None, _timer: None });
}

struct Pump {
    controller: Option<Rc<MainController>>,
    _timer: Option<Rc<DispatcherTimer>>,
}

/// Drains the core driver's messages on the UI thread: a dispatcher timer polls the
/// std channel — the sanctioned bridge, since FerroUI object handles are not `Send`
/// (verified against the pin: `DispatcherTimer::with_callback`).
fn start_pump(controller: &Rc<MainController>) {
    PUMP.with(|pump| {
        let mut pump = pump.borrow_mut();
        pump.controller = Some(Rc::clone(controller));
        pump._timer = Some(DispatcherTimer::with_callback(
            PUMP_INTERVAL,
            DispatcherPriority::BACKGROUND,
            |_| pump_tick(),
        ));
    });
}

fn pump_tick() {
    let messages: Vec<UiMsg> = core_host().ui_rx.lock().expect("ui pump mutex").try_iter().collect();
    if let Some(controller) = PUMP.with(|pump| pump.borrow().controller.clone()) {
        for message in messages {
            controller.apply(message);
        }
    }
}

