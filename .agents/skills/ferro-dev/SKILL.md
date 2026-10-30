---
name: ferro-dev
description: Writing FerroUI application code. Use when developing any FerroUI app code — the app shell (App class, AppBuilder, XAML compilation), choosing or composing controls, declarative components (rsx!, hooks, signals), XAML user controls, custom controls, styling, theming, data binding, or MVVM.
---

# FerroUI development

Everything for writing FerroUI app code in one place. Framework-level questions (crates, features, version pin, source resolution) are out of scope here: anchor first with the ferro-crates skill ("Anchor to the pin"), then return.

## App shell

1. Scaffold from `samples/HelloWorld` in the repo content cache — copy its shape, never recall it: dependency list, `build.rs`, `App.xaml` + App class, `register_types`, `program.rs`. Gotchas: compiled markup needs `ferroui-base` feature `markup-functions`; replace the sample's `SampleBuild` with `ferroui-build`.
2. Wire `Cargo.toml`: base, controls, markup, markup-xaml, one theme, desktop — one exact pin; `ferroui-build` as build-dependency.
3. The App class carries three macros and two trait methods:

```rust
ferro_class!(App: Application);
ferro_class_info!(App { new: App::new });
ferro_impl_classes!(App: FerroObjectImpl);
xaml_class!(App, "/App.xaml");          // binds the compiled markup

impl ApplicationImpl for App {
    fn initialize(this: &Self) {
        crate::register_types();         // every XAML-referenced class registers here
        this.initialize_component();
    }
    fn on_framework_initialization_completed(this: &Self) {
        // classic desktop lifetime: hand it the main window
    }
}
```

4. Entry point:

```rust
let code = AppBuilder::configure::<App>()
    .use_platform_detect()
    .start_with_classic_desktop_lifetime(&args);
```

The chain is version-sensitive — verify methods against the pin's `app_builder.rs` (`after_setup`, `use_*_subsystem`, `setup_with_lifetime`) and desktop extensions (`run`, `run_with_main_window`). Theme extensions live on theme crates (e.g. Material's `.with_material_fonts()`); ReactiveUI integration: `.use_reactive_ui(..)`.
5. Theme in `App.xaml`: `<Application.Styles><WinUITheme /></Application.Styles>`.
6. XAML facts: root `xmlns="https://github.com/ferroui"`, `x:` for the XAML namespace, `x:Class` binds code-behind; markup errors surface as **build-script errors** carrying the `.xaml` file and line — fix the markup, never the generated path; embedded resources use `ferres://` (`<StyleInclude Source="ferres://App/SideBar.xaml"/>`).

## Control inventory (crate `ferroui-controls` unless noted)

| Group | Controls |
|---|---|
| Layout | `Grid` (Column/RowDefinitions), `StackPanel`, `WrapPanel`, `DockPanel`, `RelativePanel`, `Canvas`, `FlexPanel`, `UniformGrid`, `Border`, `Viewbox`, `ScrollViewer`, `SplitView`, `Expander`, `GroupBox`, `Separator`, `TabControl`/`TabItem`, `TransitioningContentControl`, `ContentControl`, `ContentPresenter` |
| Text | `TextBlock`, `SelectableTextBlock`, `TextBox`, `MaskedTextBox`, `AutoCompleteBox`, `Label` |
| Buttons | `Button`, `RepeatButton`, `ToggleButton`, `CheckBox`, `RadioButton`, `HyperlinkButton`, `DropDownButton`, `SplitButton`, `ButtonSpinner`, `CommandBar` |
| Items | `ListBox`, `ItemsControl`, `ComboBox`, `Carousel`, `TreeView`, `TableView`, `PipsPager`, `VirtualizingStackPanel`, virtualized item templates via `template(\|item\| ..)` |
| Menus & flyouts | `Menu`, `MenuItem`, `ContextMenu`, `NativeMenu`/`NativeMenuBar`, `Flyout`, `MenuFlyout`, `Popup`, `ToolTip` |
| Pickers & range | `Calendar`, `CalendarDatePicker`, `DatePicker`, `TimePicker`, `NumericUpDown`, `Slider`, `ProgressBar`, `ToggleSwitch`, `RangeBase`, `RefreshContainer` |
| Media & draw | `Image`, Shapes (`Path`, `Rectangle`, `Ellipse`, `Line`, `Polygon`, `Polyline`, `Arc`, `Sector`), icons (`FontIcon`, `SymbolIcon`, `PathIcon`, `IconSource`), `ExperimentalAcrylicBorder`, `NativeControlHost`, `TrayIcon`, window notifications |
| Shells | `Window`, `TopLevel`, `UserControl`, `Page`, `ThemeVariantScope` |
| Extended crates | Charts (`Chart`, `LineSeries`, `BarSeries`, `AreaSeries`, axes), ColorPicker (`ColorPicker`, `ColorSpectrum`, `ColorSlider`), `InkCanvas`, TreeDataGrid, Dialogs |
| Behaviors | `ferroui-xaml-behaviors` family: triggers, actions, ~330 behaviors |

The set grows; exact membership at the pin: `register_types.rs` or docs.rs.

## Three ways to write UI

| Need | Form | Sample (repo content, `samples/`) |
|---|---|---|
| View from state | Declarative component | `HelloWorld.Hooks`, `TodoMVC`, `SevenGuis` |
| Reusable view with code-behind | XAML UserControl | `ControlCatalog` pages |
| A control others style/theme | Control subclass + ControlTheme | `SampleControls`, theme crate sources |

Default to declarative for new UI; full custom control only when the inventory lacks the primitive. The forms interoperate (`samples/ViewInterop`).

## The declarative layer (`ferroui-view`)

Two models. **Run-once** (SolidJS/Leptos): properties follow signals, updating without rerunning. **Rerender** (React): the function reruns and the reconciler patches real controls:

```rust
#[component(rerender)]
fn Counter() -> Element {
    let count = use_signal(|| 0);
    rsx! {
        StackPanel { spacing: 8.0,
            TextBlock { text: format!("Clicked {} times", count.get()) }
            Button { class: "accent", on_click: move |_| count.update(|c| *c += 1), "Click" }
        }
    }
}
// launch without XAML:
ferroui_view::launch(|| rsx! { Window { title: "..", Counter {} } });
```

| Area | API |
|---|---|
| Builders | `button()`, `stack_panel().child(..)`, `.set(property, value)`, `.bind(property, binding)` — generated per control |
| rsx! | Elements, attributes, attached properties (`Grid.row: 1`), text, conditions, loops |
| State | `signal`, `memo`, `effect`, owners; `text: count` follows a signal; `name.two_way()` |
| Children | `for_each` (keyed), `show`/`show_else`, `dynamic` |
| Components | `#[component]` runs once; `#[component(rerender)]` reruns; comparable parameters memoize by default (`always` opts out) |
| Hooks | `use_signal`, `use_memo`, `use_effect` (follows signals it reads), `use_effect_with(deps, ..)`, `use_hook`, `provide_context`, `use_context::<T>()` |
| Async | `use_future(deps, \|deps\| future)`, `suspense(..)`, `AsyncState` |
| Errors | `error_boundary(\|\| child, \|info\| fallback)`, `info.reset()` |
| Navigation | `router("/", vec![route("/people/:id", \|m\| ..)])`, `use_router().navigate(..)`, `navigate_back`, `navigate_and_reset` |
| XAML interop | `ComponentHost`, `use_xaml`, `register_component`, `use_data_context` |
| Motion | `transition(property, ms(200))`, `transitions([..])`, `animation(..)`, `use_animation` |
| Popups | `Popup { .. }` as element; `context_menu: rsx! { .. }` |
| Tools | hot reload (`hot-reload` feature), diagnostics, inspector Shift+F12 (`open_inspector`) |

From React: JSX → `rsx!`; `useState` → `use_signal`; `useEffect` → `use_effect_with` + `on_cleanup`; `useMemo` → `use_memo`; `useContext` → `provide_context`/`use_context`; portals → `Popup`; Suspense/Router → `use_future` + `suspense` / `router`.

## Styling and theming

- Styles in `Application.Styles`, on a window/control, or `<StyleInclude Source="ferres://..."/>`:

```xml
<Style Selector="Button.accent">
  <Setter Property="Background" Value="..."/>
</Style>
```

- Selector grammar: type, `.class`, `#name`, pseudo-class (`:pointerover`, `:pressed`, `:checked`), combinators (descendant space, child `>`), chained (`Button.accent:pointerover`); later overrides earlier at equal specificity. In rsx: `styles: [..]`, `style("Button.accent").set(..)`, `class: "accent"`.
- ControlTheme = a control's default template + property defaults. The theme crates (`ferroui-themes-winui` default, `-fluent`, `-simple`, `-material`, `-macos`) ship one XAML set per control — the reference for theming any control (`Resources/Button_themeresources.xaml`). Theming a custom control: ControlTheme targeting the subclass, template with `TemplateBinding`s, states as pseudo-classes — shape from `samples/SampleControls`.
- Theme variants (light/dark) via `ThemeVariantScope`, `use_theme_variant`; resources resolve up the logical tree, `{DynamicResource Key}` follows variant switches; `use_resource_as::<T>(key)` in components. Resource keys are version-sensitive — take them from the pinned theme's XAML.
- Debugging: F12 devtools Styles view shows which selector/theme/resource supplied each value.

## Data binding and MVVM

- XAML paths (shapes from the pinned ControlCatalog):

```xml
<CheckBox IsChecked="{Binding #DemoButton.IsEnabled}" />
<Button Command="{Binding $parent[controls:SamplePage].((pages:ButtonsPage)DataContext).CountCommand}" />
```

`{Binding Path}` on the DataContext; `#Name` element lookup; `$parent[Type]` tree walk with `((ns:Type)...)` casts. `x:CompileBindings="True"` type-checks paths at compile time — prefer for app code.
- In rsx: `.bind(property, binding)`, `.two_way()`, or skip bindings — `text: signal` keeps a property following state.
- Converters transform through the path; validation errors surface via `DataValidationErrors`.
- MVVM stack: `ferroui-reactive`, `ferroui-dynamicdata` (reactive collections), `ferroui-reactiveui` (view models, commands), wired with `.use_reactive_ui(..)`. View models test without a window; threading discipline in `docs/mvvm.md` §9. Choosing: local state → signal; view model/commands/validation → bindings + ReactiveUI; collections with filter/sort → DynamicData.

## Procedure and bar

1. Anchor to the pin; open the branch's canonical sample in the repo content cache.
2. Keep the sample's wiring; rename content only.
3. Verify unfamiliar controls' properties/events in the pinned source before use; build, run, exercise — F12 devtools for property origins.
4. Bar: every control, property, event, hook, selector, and binding path in your code appears in the pinned version's sources or samples.
