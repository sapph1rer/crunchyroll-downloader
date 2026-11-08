//! A dropdown whose list floats over the page. The field keeps its place in the layout,
//! and the list is drawn as an overlay just below it. Opening the list never pushes the
//! content under it down, and a click outside both the field and the list closes it.
//! When there is no room below the field, the list opens above it instead.

use iced::advanced::layout::{self, Layout};
use iced::advanced::overlay;
use iced::advanced::renderer;
use iced::advanced::widget::{Operation, Tree, Widget};
use iced::advanced::{Clipboard, Shell};
use iced::{mouse, Element, Event, Length, Point, Rectangle, Renderer, Size, Theme, Vector};

/// A field whose list floats below it while `visible` is set.
pub struct Dropdown<'a, Message> {
    field: Element<'a, Message>,
    list: Element<'a, Message>,
    visible: bool,
    /// Space between the field and the list.
    gap: f32,
    /// Sent when a click lands outside both the field and the list.
    on_dismiss: Message,
}

impl<'a, Message: Clone> Dropdown<'a, Message> {
    pub fn new(
        field: impl Into<Element<'a, Message>>,
        list: impl Into<Element<'a, Message>>,
        visible: bool,
        gap: f32,
        on_dismiss: Message,
    ) -> Self {
        Self { field: field.into(), list: list.into(), visible, gap, on_dismiss }
    }
}

impl<'a, Message: Clone + 'a> From<Dropdown<'a, Message>> for Element<'a, Message> {
    fn from(dropdown: Dropdown<'a, Message>) -> Self {
        Element::new(dropdown)
    }
}

impl<Message: Clone> Widget<Message, Theme, Renderer> for Dropdown<'_, Message> {
    fn size(&self) -> Size<Length> {
        self.field.as_widget().size()
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.field), Tree::new(&self.list)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(&[self.field.as_widget(), self.list.as_widget()]);
    }

    fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        // Only the field takes space in the page. The list is laid out in the overlay.
        self.field.as_widget_mut().layout(&mut tree.children[0], renderer, limits)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.field.as_widget().draw(&tree.children[0], renderer, theme, style, layout, cursor, viewport);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.field.as_widget_mut().update(&mut tree.children[0], event, layout, cursor, renderer, clipboard, shell, viewport);
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.field.as_widget().mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }

    fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer, operation: &mut dyn Operation) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            self.field.as_widget_mut().operate(&mut tree.children[0], layout, renderer, operation);
        });
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        let position = layout.position() + translation;
        let size = layout.bounds().size();
        let field = Rectangle { x: position.x, y: position.y, width: size.width, height: size.height };
        let (field_tree, list_tree) = tree.children.split_at_mut(1);
        let field_overlay = self.field.as_widget_mut().overlay(&mut field_tree[0], layout, renderer, viewport, translation);
        if !self.visible {
            return field_overlay;
        }
        let list = overlay::Element::new(Box::new(ListOverlay {
            position,
            field,
            list: &mut self.list,
            tree: &mut list_tree[0],
            gap: self.gap,
            on_dismiss: self.on_dismiss.clone(),
        }));
        let mut parts: Vec<overlay::Element<'b, Message, Theme, Renderer>> = field_overlay.into_iter().collect();
        parts.push(list);
        Some(overlay::Group::with_children(parts).overlay())
    }
}

/// The list, placed below the field (or above it when the window has no room below).
struct ListOverlay<'a, 'b, Message> {
    /// Top-left corner of the field, in window coordinates.
    position: Point,
    field: Rectangle,
    list: &'b mut Element<'a, Message>,
    tree: &'b mut Tree,
    gap: f32,
    on_dismiss: Message,
}

impl<Message: Clone> overlay::Overlay<Message, Theme, Renderer> for ListOverlay<'_, '_, Message> {
    fn layout(&mut self, renderer: &Renderer, bounds: Size) -> layout::Node {
        // The list is as wide as the field. Its height comes from its own (animated) size.
        let width = self.field.width;
        let limits = layout::Limits::new(Size::new(width, 0.0), Size::new(width, bounds.height.max(0.0)));
        let node = self.list.as_widget_mut().layout(self.tree, renderer, &limits);
        let size = node.size();
        let below = self.position.y + self.field.height + self.gap;
        let above = self.position.y - self.gap - size.height;
        // Open upward only when the list does not fit below and does fit above.
        let y = if below + size.height > bounds.height && above >= 0.0 { above } else { below };
        let x = self.position.x.clamp(0.0, (bounds.width - width).max(0.0));
        layout::Node::with_children(size, vec![node]).move_to(Point::new(x, y))
    }

    fn draw(
        &self,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
    ) {
        let child = layout.children().next().unwrap();
        self.list
            .as_widget()
            .draw(self.tree, renderer, theme, style, child, cursor, &Rectangle::with_size(Size::INFINITE));
    }

    fn operate(&mut self, layout: Layout<'_>, renderer: &Renderer, operation: &mut dyn Operation) {
        let child = layout.children().next().unwrap();
        self.list.as_widget_mut().operate(self.tree, child, renderer, operation);
    }

    fn update(
        &mut self,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
    ) {
        let child = layout.children().next().unwrap();
        // A press outside the list closes it. A press on the field is left to the field,
        // which toggles the list itself.
        if let Event::Mouse(mouse::Event::ButtonPressed(_)) = event {
            if !cursor.is_over(child.bounds()) && !cursor.is_over(self.field) {
                shell.publish(self.on_dismiss.clone());
            }
        }
        self.list
            .as_widget_mut()
            .update(self.tree, event, child, cursor, renderer, clipboard, shell, &layout.bounds());
    }

    fn mouse_interaction(&self, layout: Layout<'_>, cursor: mouse::Cursor, renderer: &Renderer) -> mouse::Interaction {
        let child = layout.children().next().unwrap();
        self.list.as_widget().mouse_interaction(self.tree, child, cursor, &layout.bounds(), renderer)
    }
}
