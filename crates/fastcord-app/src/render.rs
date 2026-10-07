//! Native GPU selection from ADR 0002, without process-environment mutation.
//!
//! Use `render::application(boot, update, view)` in place of `iced::application`;
//! all builder methods and existing `iced::Element` view signatures stay intact.
//! iced 0.14's default compositor reads `WGPU_BACKEND`, so its public
//! `wgpu::window::Compositor::request` settings API is used instead. A root-widget
//! bridge keeps renderer-specific widgets (notably iced's QR code) compatible.

use iced::application::{Application, BootFn, UpdateFn, ViewFn};
use iced::{Element, Program, Theme};
use iced_renderer::core::{self, layout, mouse, overlay, renderer, text, widget};
use iced_renderer::graphics::{self, compositor};
use iced_renderer::wgpu::{self as gpu, wgpu};

use std::borrow::Cow;

#[cfg(target_os = "windows")]
const NATIVE_BACKEND: wgpu::Backends = wgpu::Backends::DX12;
#[cfg(target_os = "macos")]
const NATIVE_BACKEND: wgpu::Backends = wgpu::Backends::METAL;
#[cfg(target_os = "linux")]
const NATIVE_BACKEND: wgpu::Backends = wgpu::Backends::VULKAN;
#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
compile_error!("fastcord's native renderer supports Windows, macOS, and Linux only");

/// Builds an iced application with the OS-native GPU API pinned at creation.
///
/// `WGPU_BACKEND` and `ICED_BACKEND` cannot select another GPU API or force CPU
/// rendering. The built-in tiny-skia compositor is tried only when no adapter
/// exists for the native API; surface/device errors are returned to the caller.
pub fn application<State, Message>(
    boot: impl BootFn<State, Message>,
    update: impl UpdateFn<State, Message>,
    view: impl for<'a> ViewFn<'a, State, Message, Theme, iced::Renderer>,
) -> Application<impl Program<State = State, Message = Message, Theme = Theme>>
where
    State: 'static,
    Message: Send + 'static,
{
    iced::application(boot, update, NativeView(view))
}

fn gpu_settings(settings: graphics::Settings) -> gpu::Settings {
    gpu::Settings {
        backends: NATIVE_BACKEND,
        ..settings.into()
    }
}

fn headless_instance_descriptor() -> wgpu::InstanceDescriptor {
    wgpu::InstanceDescriptor {
        backends: NATIVE_BACKEND,
        flags: wgpu::InstanceFlags::empty(),
        ..Default::default()
    }
}

fn adapter_missing(error: &gpu::window::compositor::Error) -> bool {
    matches!(error, gpu::window::compositor::Error::NoAdapterFound(_))
}

// The newtype changes iced's associated compositor, not its rendering engine.
struct NativeRenderer(iced::Renderer);
struct NativeCompositor(iced_renderer::Compositor);

impl compositor::Default for NativeRenderer {
    type Compositor = NativeCompositor;
}

impl graphics::Compositor for NativeCompositor {
    type Renderer = NativeRenderer;
    type Surface = <iced_renderer::Compositor as graphics::Compositor>::Surface;

    async fn with_backend(
        settings: graphics::Settings,
        display: impl compositor::Display + Clone,
        compatible_window: impl compositor::Window + Clone,
        shell: graphics::Shell,
        _backend: Option<&str>,
    ) -> Result<Self, graphics::Error> {
        let mut native_settings = gpu_settings(settings);

        // Preserve iced's presentation-mode override; it cannot change the API.
        if let Some(present_mode) = gpu::settings::present_mode_from_env() {
            native_settings.present_mode = present_mode;
        }

        match gpu::window::Compositor::request(
            native_settings,
            Some(compatible_window.clone()),
            shell.clone(),
        )
        .await
        {
            Ok(compositor) => Ok(Self(iced_renderer::fallback::Compositor::Primary(
                compositor,
            ))),
            Err(error) if adapter_missing(&error) => {
                // An explicit preference bypasses ICED_BACKEND. The wgpu branch
                // rejects "tiny-skia" before initialization, so no other GPU API
                // is requested by the built-in fallback compositor.
                iced_renderer::Compositor::with_backend(
                    settings,
                    display,
                    compatible_window,
                    shell,
                    Some("tiny-skia"),
                )
                .await
                .map(Self)
                .map_err(|fallback| graphics::Error::List(vec![error.into(), fallback]))
            }
            Err(error) => Err(error.into()),
        }
    }

    fn create_renderer(&self) -> Self::Renderer {
        NativeRenderer(self.0.create_renderer())
    }

    fn create_surface<W: compositor::Window + Clone>(
        &mut self,
        window: W,
        width: u32,
        height: u32,
    ) -> Self::Surface {
        self.0.create_surface(window, width, height)
    }

    fn configure_surface(&mut self, surface: &mut Self::Surface, width: u32, height: u32) {
        self.0.configure_surface(surface, width, height);
    }

    fn load_font(&mut self, font: Cow<'static, [u8]>) {
        self.0.load_font(font);
    }

    fn information(&self) -> compositor::Information {
        self.0.information()
    }

    fn present(
        &mut self,
        renderer: &mut Self::Renderer,
        surface: &mut Self::Surface,
        viewport: &graphics::Viewport,
        background_color: core::Color,
        on_pre_present: impl FnOnce(),
    ) -> Result<(), compositor::SurfaceError> {
        self.0.present(
            &mut renderer.0,
            surface,
            viewport,
            background_color,
            on_pre_present,
        )
    }

    fn screenshot(
        &mut self,
        renderer: &mut Self::Renderer,
        viewport: &graphics::Viewport,
        background_color: core::Color,
    ) -> Vec<u8> {
        self.0
            .screenshot(&mut renderer.0, viewport, background_color)
    }
}

impl core::Renderer for NativeRenderer {
    fn start_layer(&mut self, bounds: core::Rectangle) {
        self.0.start_layer(bounds);
    }

    fn end_layer(&mut self) {
        self.0.end_layer();
    }

    fn start_transformation(&mut self, transformation: core::Transformation) {
        self.0.start_transformation(transformation);
    }

    fn end_transformation(&mut self) {
        self.0.end_transformation();
    }

    fn fill_quad(&mut self, quad: renderer::Quad, background: impl Into<core::Background>) {
        self.0.fill_quad(quad, background);
    }

    fn reset(&mut self, new_bounds: core::Rectangle) {
        self.0.reset(new_bounds);
    }

    fn allocate_image(
        &mut self,
        handle: &core::image::Handle,
        callback: impl FnOnce(Result<core::image::Allocation, core::image::Error>) + Send + 'static,
    ) {
        self.0.allocate_image(handle, callback);
    }
}

impl text::Renderer for NativeRenderer {
    type Font = core::Font;
    type Paragraph = <iced::Renderer as text::Renderer>::Paragraph;
    type Editor = <iced::Renderer as text::Renderer>::Editor;

    const ICON_FONT: Self::Font = <iced::Renderer as text::Renderer>::ICON_FONT;
    const CHECKMARK_ICON: char = <iced::Renderer as text::Renderer>::CHECKMARK_ICON;
    const ARROW_DOWN_ICON: char = <iced::Renderer as text::Renderer>::ARROW_DOWN_ICON;
    const SCROLL_UP_ICON: char = <iced::Renderer as text::Renderer>::SCROLL_UP_ICON;
    const SCROLL_DOWN_ICON: char = <iced::Renderer as text::Renderer>::SCROLL_DOWN_ICON;
    const SCROLL_LEFT_ICON: char = <iced::Renderer as text::Renderer>::SCROLL_LEFT_ICON;
    const SCROLL_RIGHT_ICON: char = <iced::Renderer as text::Renderer>::SCROLL_RIGHT_ICON;
    const ICED_LOGO: char = <iced::Renderer as text::Renderer>::ICED_LOGO;

    fn default_font(&self) -> Self::Font {
        self.0.default_font()
    }

    fn default_size(&self) -> core::Pixels {
        self.0.default_size()
    }

    fn fill_paragraph(
        &mut self,
        paragraph: &Self::Paragraph,
        position: core::Point,
        color: core::Color,
        clip_bounds: core::Rectangle,
    ) {
        self.0
            .fill_paragraph(paragraph, position, color, clip_bounds);
    }

    fn fill_editor(
        &mut self,
        editor: &Self::Editor,
        position: core::Point,
        color: core::Color,
        clip_bounds: core::Rectangle,
    ) {
        self.0.fill_editor(editor, position, color, clip_bounds);
    }

    fn fill_text(
        &mut self,
        text: text::Text<String, Self::Font>,
        position: core::Point,
        color: core::Color,
        clip_bounds: core::Rectangle,
    ) {
        self.0.fill_text(text, position, color, clip_bounds);
    }
}

// iced::Program requires Headless even for a windowed application. Its built-in
// wgpu implementation also reads WGPU_BACKEND, so this path must be pinned too.
impl renderer::Headless for NativeRenderer {
    async fn new(
        default_font: core::Font,
        default_text_size: core::Pixels,
        _backend: Option<&str>,
    ) -> Option<Self> {
        let instance = wgpu::Instance::new(&headless_instance_descriptor());
        let adapter = match instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: None,
            })
            .await
        {
            Ok(adapter) => adapter,
            Err(_) => {
                return <iced::Renderer as renderer::Headless>::new(
                    default_font,
                    default_text_size,
                    Some("tiny-skia"),
                )
                .await
                .map(Self);
            }
        };

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("fastcord native headless renderer"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits {
                    max_bind_groups: 2,
                    ..wgpu::Limits::default()
                },
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                trace: wgpu::Trace::Off,
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
            })
            .await
            .ok()?;

        let format = if graphics::color::GAMMA_CORRECTION {
            wgpu::TextureFormat::Rgba8UnormSrgb
        } else {
            wgpu::TextureFormat::Rgba8Unorm
        };
        let engine = gpu::Engine::new(
            &adapter,
            device,
            queue,
            format,
            Some(graphics::Antialiasing::MSAAx4),
            graphics::Shell::headless(),
        );

        Some(Self(iced_renderer::fallback::Renderer::Primary(
            gpu::Renderer::new(engine, default_font, default_text_size),
        )))
    }

    fn name(&self) -> String {
        self.0.name()
    }

    fn screenshot(
        &mut self,
        size: core::Size<u32>,
        scale_factor: f32,
        background_color: core::Color,
    ) -> Vec<u8> {
        self.0.screenshot(size, scale_factor, background_color)
    }
}

struct NativeView<V>(V);

impl<'a, State, Message, V> ViewFn<'a, State, Message, Theme, NativeRenderer> for NativeView<V>
where
    Message: 'a,
    V: ViewFn<'a, State, Message, Theme, iced::Renderer>,
{
    fn view(&self, state: &'a State) -> core::Element<'a, Message, Theme, NativeRenderer> {
        core::Element::new(NativeWidget(self.0.view(state)))
    }
}

// Bridge only the root, not every child. Forward the original tree unchanged so
// focus, scrolling, QR geometry caches, and all widget state survive rebuilding.
struct NativeWidget<'a, Message>(Element<'a, Message>);

impl<'a, Message: 'a> core::Widget<Message, Theme, NativeRenderer> for NativeWidget<'a, Message> {
    fn size(&self) -> core::Size<core::Length> {
        self.0.as_widget().size()
    }

    fn size_hint(&self) -> core::Size<core::Length> {
        self.0.as_widget().size_hint()
    }

    fn tag(&self) -> widget::tree::Tag {
        self.0.as_widget().tag()
    }

    fn state(&self) -> widget::tree::State {
        self.0.as_widget().state()
    }

    fn children(&self) -> Vec<widget::Tree> {
        self.0.as_widget().children()
    }

    fn diff(&self, tree: &mut widget::Tree) {
        self.0.as_widget().diff(tree);
    }

    fn layout(
        &mut self,
        tree: &mut widget::Tree,
        renderer: &NativeRenderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.0.as_widget_mut().layout(tree, &renderer.0, limits)
    }

    fn operate(
        &mut self,
        tree: &mut widget::Tree,
        layout: core::Layout<'_>,
        renderer: &NativeRenderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.0
            .as_widget_mut()
            .operate(tree, layout, &renderer.0, operation);
    }

    fn update(
        &mut self,
        tree: &mut widget::Tree,
        event: &core::Event,
        layout: core::Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &NativeRenderer,
        clipboard: &mut dyn core::Clipboard,
        shell: &mut core::Shell<'_, Message>,
        viewport: &core::Rectangle,
    ) {
        self.0.as_widget_mut().update(
            tree,
            event,
            layout,
            cursor,
            &renderer.0,
            clipboard,
            shell,
            viewport,
        );
    }

    fn draw(
        &self,
        tree: &widget::Tree,
        renderer: &mut NativeRenderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: core::Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &core::Rectangle,
    ) {
        self.0.as_widget().draw(
            tree,
            &mut renderer.0,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &widget::Tree,
        layout: core::Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &core::Rectangle,
        renderer: &NativeRenderer,
    ) -> mouse::Interaction {
        self.0
            .as_widget()
            .mouse_interaction(tree, layout, cursor, viewport, &renderer.0)
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut widget::Tree,
        layout: core::Layout<'b>,
        renderer: &NativeRenderer,
        viewport: &core::Rectangle,
        translation: core::Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, NativeRenderer>> {
        self.0
            .as_widget_mut()
            .overlay(tree, layout, &renderer.0, viewport, translation)
            .map(|overlay| overlay::Element::new(Box::new(NativeOverlay(overlay))))
    }
}

struct NativeOverlay<'a, Message>(overlay::Element<'a, Message, Theme, iced::Renderer>);

impl<'a, Message: 'a> core::Overlay<Message, Theme, NativeRenderer> for NativeOverlay<'a, Message> {
    fn layout(&mut self, renderer: &NativeRenderer, bounds: core::Size) -> layout::Node {
        self.0.as_overlay_mut().layout(&renderer.0, bounds)
    }

    fn draw(
        &self,
        renderer: &mut NativeRenderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: core::Layout<'_>,
        cursor: mouse::Cursor,
    ) {
        self.0
            .as_overlay()
            .draw(&mut renderer.0, theme, style, layout, cursor);
    }

    fn operate(
        &mut self,
        layout: core::Layout<'_>,
        renderer: &NativeRenderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.0
            .as_overlay_mut()
            .operate(layout, &renderer.0, operation);
    }

    fn update(
        &mut self,
        event: &core::Event,
        layout: core::Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &NativeRenderer,
        clipboard: &mut dyn core::Clipboard,
        shell: &mut core::Shell<'_, Message>,
    ) {
        self.0
            .as_overlay_mut()
            .update(event, layout, cursor, &renderer.0, clipboard, shell);
    }

    fn mouse_interaction(
        &self,
        layout: core::Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &NativeRenderer,
    ) -> mouse::Interaction {
        self.0
            .as_overlay()
            .mouse_interaction(layout, cursor, &renderer.0)
    }

    fn overlay<'b>(
        &'b mut self,
        layout: core::Layout<'b>,
        renderer: &NativeRenderer,
    ) -> Option<overlay::Element<'b, Message, Theme, NativeRenderer>> {
        self.0
            .as_overlay_mut()
            .overlay(layout, &renderer.0)
            .map(|overlay| overlay::Element::new(Box::new(NativeOverlay(overlay))))
    }

    fn index(&self) -> f32 {
        self.0.as_overlay().index()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::widget::{column, qr_code, text, text_input};

    fn assert_native_backend(backends: wgpu::Backends) {
        #[cfg(target_os = "windows")]
        assert_eq!(backends, wgpu::Backends::DX12);
        #[cfg(target_os = "macos")]
        assert_eq!(backends, wgpu::Backends::METAL);
        #[cfg(target_os = "linux")]
        assert_eq!(backends, wgpu::Backends::VULKAN);
        assert_eq!(backends.bits().count_ones(), 1);
        assert!(!backends.contains(wgpu::Backends::GL));
    }

    #[test]
    fn windowed_and_headless_configuration_use_only_the_native_api() {
        assert_native_backend(gpu_settings(graphics::Settings::default()).backends);
        assert_native_backend(headless_instance_descriptor().backends);
    }

    #[test]
    fn pinning_preserves_font_text_antialiasing_and_vsync_settings() {
        let settings = graphics::Settings {
            default_font: core::Font::MONOSPACE,
            default_text_size: core::Pixels(21.0),
            antialiasing: Some(graphics::Antialiasing::MSAAx4),
            vsync: false,
        };
        let pinned = gpu_settings(settings);

        assert_native_backend(pinned.backends);
        assert_eq!(pinned.default_font, settings.default_font);
        assert_eq!(pinned.default_text_size, settings.default_text_size);
        assert_eq!(pinned.antialiasing, settings.antialiasing);
        assert_eq!(pinned.present_mode, wgpu::PresentMode::AutoNoVsync);
        assert_eq!(
            gpu_settings(graphics::Settings::default()).present_mode,
            wgpu::PresentMode::AutoVsync
        );
    }

    #[test]
    fn software_fallback_requires_a_missing_adapter() {
        use gpu::window::compositor::Error;

        assert!(adapter_missing(&Error::NoAdapterFound(
            "no supported native adapter".to_owned()
        )));
        assert!(!adapter_missing(&Error::IncompatibleSurface));
        assert!(!adapter_missing(&Error::RequestDeviceFailed(Vec::new())));
    }

    #[test]
    fn backend_environment_cannot_override_configuration() {
        const PROBE: &str = "FASTCORD_NATIVE_RENDERER_ENV_TEST";

        if std::env::var_os(PROBE).is_some() {
            assert_eq!(std::env::var("WGPU_BACKEND").as_deref(), Ok("gl"));
            assert_eq!(std::env::var("ICED_BACKEND").as_deref(), Ok("tiny-skia"));
            assert_native_backend(gpu_settings(graphics::Settings::default()).backends);
            assert_native_backend(headless_instance_descriptor().backends);
            return;
        }

        // Command::env affects only the child process and is safe under the
        // edition-2024 unsafe-code ban; the test runner's environment is intact.
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "render::tests::backend_environment_cannot_override_configuration",
            ])
            .env(PROBE, "1")
            .env("WGPU_BACKEND", "gl")
            .env("ICED_BACKEND", "tiny-skia")
            .output()
            .expect("renderer environment probe starts");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(output.status.success(), "{stdout}\n{stderr}");
        assert!(stdout.contains("1 passed"), "probe must run: {stdout}");
    }

    #[test]
    fn builder_preserves_callbacks_and_application_settings() {
        fn view(value: &u32) -> Element<'_, u32> {
            text(value).into()
        }

        let application = application(
            || 7_u32,
            |value: &mut u32, message: u32| *value += message,
            view,
        )
        .title("fastcord native renderer")
        .window_size((320, 200))
        .default_font(core::Font::MONOSPACE)
        .antialiasing(true);
        let (mut value, _) = Program::boot(&application);
        assert_eq!(value, 7);
        let _ = Program::update(&application, &mut value, 3);
        assert_eq!(value, 10);

        let settings = Program::settings(&application);
        assert_eq!(settings.default_font, core::Font::MONOSPACE);
        assert!(settings.antialiasing);
        assert_eq!(
            Program::window(&application).expect("initial window").size,
            core::Size::new(320.0, 200.0)
        );
        let window = iced::window::Id::unique();
        assert_eq!(
            Program::title(&application, &value, window),
            "fastcord native renderer"
        );
        let bridged = Program::view(&application, &value, window);
        let original = view(&value);
        assert_eq!(bridged.as_widget().tag(), original.as_widget().tag());
        assert_eq!(bridged.as_widget().size(), original.as_widget().size());
    }

    #[test]
    fn root_bridge_accepts_builtin_qr_and_preserves_widget_children() {
        fn view(data: &qr_code::Data) -> Element<'_, ()> {
            column![
                qr_code(data),
                text_input("Token", ""),
                text("Offline QR test")
            ]
            .into()
        }

        let application = application(
            || qr_code::Data::new("https://discord.com/ra/offline-renderer-test").expect("QR data"),
            |_: &mut qr_code::Data, _: ()| {},
            view,
        );
        let (data, _) = Program::boot(&application);
        let bridged = Program::view(&application, &data, iced::window::Id::unique());
        let original = view(&data);

        assert_eq!(bridged.as_widget().tag(), original.as_widget().tag());
        assert_eq!(
            bridged.as_widget().size_hint(),
            original.as_widget().size_hint()
        );
        assert_eq!(bridged.as_widget().children().len(), 3);
        assert_eq!(
            bridged.as_widget().children().len(),
            original.as_widget().children().len()
        );
    }
}
