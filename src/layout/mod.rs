//! Layouting infrastructure.

mod constraints;
mod incremental;
mod regions;

pub use constraints::*;
pub use incremental::*;
pub use regions::*;

use std::any::{Any, TypeId};
use std::fmt::{self, Debug, Formatter};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use crate::diag::TypResult;
use crate::eval::StyleChain;
use crate::frame::{Element, Frame, Geometry, Shape, Stroke};
use crate::geom::{Align, Linear, Paint, Point, Sides, Size, Spec, Transform};
use crate::library::{AlignNode, PadNode, TransformNode, MOVE};
use crate::util::Prehashed;
use crate::Vm;

/// A node that can be layouted into a sequence of regions.
///
/// Layout return one frame per used region alongside constraints that define
/// whether the result is reusable in other regions.
pub trait Layout {
    /// Layout the node into the given regions, producing constrained frames.
    fn layout(
        &self,
        vm: &mut Vm,
        regions: &Regions,
        styles: StyleChain,
    ) -> TypResult<Vec<Constrained<Arc<Frame>>>>;

    /// Convert to a packed node.
    fn pack(self) -> LayoutNode
    where
        Self: Debug + Hash + Sized + Sync + Send + 'static,
    {
        LayoutNode::new(self)
    }
}

/// A type-erased layouting node with a precomputed hash.
#[derive(Clone, Hash)]
pub struct LayoutNode(Arc<Prehashed<dyn Bounds>>);

impl LayoutNode {
    /// Pack any layoutable node.
    pub fn new<T>(node: T) -> Self
    where
        T: Layout + Debug + Hash + Sync + Send + 'static,
    {
        Self(Arc::new(Prehashed::new(node)))
    }

    /// Check whether the contained node is a specific layout node.
    pub fn is<T: 'static>(&self) -> bool {
        self.0.as_any().is::<T>()
    }

    /// The type id of this node.
    pub fn id(&self) -> TypeId {
        self.0.as_any().type_id()
    }

    /// Try to downcast to a specific layout node.
    pub fn downcast<T>(&self) -> Option<&T>
    where
        T: Layout + Debug + Hash + 'static,
    {
        self.0.as_any().downcast_ref()
    }

    /// Force a size for this node.
    pub fn sized(self, sizing: Spec<Option<Linear>>) -> Self {
        if sizing.any(Option::is_some) {
            SizedNode { sizing, child: self }.pack()
        } else {
            self
        }
    }

    /// Fill the frames resulting from a node.
    pub fn filled(self, fill: Paint) -> Self {
        FillNode { fill, child: self }.pack()
    }

    /// Stroke the frames resulting from a node.
    pub fn stroked(self, stroke: Stroke) -> Self {
        StrokeNode { stroke, child: self }.pack()
    }

    /// Set alignments for this node.
    pub fn aligned(self, aligns: Spec<Option<Align>>) -> Self {
        if aligns.any(Option::is_some) {
            AlignNode { aligns, child: self }.pack()
        } else {
            self
        }
    }

    /// Pad this node at the sides.
    pub fn padded(self, padding: Sides<Linear>) -> Self {
        if !padding.left.is_zero()
            || !padding.top.is_zero()
            || !padding.right.is_zero()
            || !padding.bottom.is_zero()
        {
            PadNode { padding, child: self }.pack()
        } else {
            self
        }
    }

    /// Transform this node's contents without affecting layout.
    pub fn moved(self, offset: Point) -> Self {
        if !offset.is_zero() {
            TransformNode::<MOVE> {
                transform: Transform::translation(offset.x, offset.y),
                child: self,
            }
            .pack()
        } else {
            self
        }
    }
}

impl Layout for LayoutNode {
    #[track_caller]
    fn layout(
        &self,
        vm: &mut Vm,
        regions: &Regions,
        styles: StyleChain,
    ) -> TypResult<Vec<Constrained<Arc<Frame>>>> {
        let styles = styles.barred(self.id());

        let hash = {
            let mut state = fxhash::FxHasher64::default();
            self.hash(&mut state);
            styles.hash(&mut state);
            state.finish()
        };

        // This is not written with `unwrap_or_else`, because then the
        // #[track_caller] annotation doesn't work.
        if let Some(frames) = vm.layout_cache.get(hash, regions) {
            Ok(frames)
        } else {
            vm.level += 1;
            let frames = self.0.layout(vm, regions, styles)?;
            vm.level -= 1;

            let entry = FramesEntry::new(frames.clone(), vm.level);

            #[cfg(debug_assertions)]
            if !entry.check(regions) {
                eprintln!("node: {:#?}", self.0);
                eprintln!("regions: {regions:#?}");
                eprintln!(
                    "constraints: {:#?}",
                    frames.iter().map(|c| c.cts).collect::<Vec<_>>(),
                );
                panic!("constraints did not match regions they were created for");
            }

            vm.layout_cache.insert(hash, entry);
            Ok(frames)
        }
    }

    fn pack(self) -> LayoutNode {
        self
    }
}

impl Default for LayoutNode {
    fn default() -> Self {
        EmptyNode.pack()
    }
}

impl Debug for LayoutNode {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl PartialEq for LayoutNode {
    fn eq(&self, other: &Self) -> bool {
        self.0.eq(&other.0)
    }
}

trait Bounds: Layout + Debug + Sync + Send + 'static {
    fn as_any(&self) -> &dyn Any;
}

impl<T> Bounds for T
where
    T: Layout + Debug + Hash + Sync + Send + 'static,
{
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A layout node that produces an empty frame.
///
/// The packed version of this is returned by [`PackedNode::default`].
#[derive(Debug, Hash)]
struct EmptyNode;

impl Layout for EmptyNode {
    fn layout(
        &self,
        _: &mut Vm,
        regions: &Regions,
        _: StyleChain,
    ) -> TypResult<Vec<Constrained<Arc<Frame>>>> {
        let size = regions.expand.select(regions.current, Size::zero());
        let mut cts = Constraints::new(regions.expand);
        cts.exact = regions.current.filter(regions.expand);
        Ok(vec![Frame::new(size).constrain(cts)])
    }
}

/// Fix the size of a node.
#[derive(Debug, Hash)]
struct SizedNode {
    /// How to size the node horizontally and vertically.
    sizing: Spec<Option<Linear>>,
    /// The node to be sized.
    child: LayoutNode,
}

impl Layout for SizedNode {
    fn layout(
        &self,
        vm: &mut Vm,
        regions: &Regions,
        styles: StyleChain,
    ) -> TypResult<Vec<Constrained<Arc<Frame>>>> {
        let is_auto = self.sizing.map_is_none();
        let is_rel = self.sizing.map(|s| s.map_or(false, Linear::is_relative));

        // The "pod" is the region into which the child will be layouted.
        let pod = {
            // Resolve the sizing to a concrete size.
            let size = self
                .sizing
                .zip(regions.base)
                .map(|(s, b)| s.map(|v| v.resolve(b)))
                .unwrap_or(regions.current);

            // Select the appropriate base and expansion for the child depending
            // on whether it is automatically or linearly sized.
            let base = is_auto.select(regions.base, size);
            let expand = regions.expand | !is_auto;

            Regions::one(size, base, expand)
        };

        let mut frames = self.child.layout(vm, &pod, styles)?;
        let Constrained { item: frame, cts } = &mut frames[0];

        // Ensure frame size matches regions size if expansion is on.
        let target = regions.expand.select(regions.current, frame.size);
        Arc::make_mut(frame).resize(target, Align::LEFT_TOP);

        // Set base & exact constraints if the child is automatically sized
        // since we don't know what the child might have done. Also set base if
        // our sizing is relative.
        *cts = Constraints::new(regions.expand);
        cts.exact = regions.current.filter(regions.expand | is_auto);
        cts.base = regions.base.filter(is_rel | is_auto);

        Ok(frames)
    }
}

/// Fill the frames resulting from a node.
#[derive(Debug, Hash)]
struct FillNode {
    /// How to fill the frames resulting from the `child`.
    fill: Paint,
    /// The node to fill.
    child: LayoutNode,
}

impl Layout for FillNode {
    fn layout(
        &self,
        vm: &mut Vm,
        regions: &Regions,
        styles: StyleChain,
    ) -> TypResult<Vec<Constrained<Arc<Frame>>>> {
        let mut frames = self.child.layout(vm, regions, styles)?;
        for Constrained { item: frame, .. } in &mut frames {
            let shape = Shape::filled(Geometry::Rect(frame.size), self.fill);
            Arc::make_mut(frame).prepend(Point::zero(), Element::Shape(shape));
        }
        Ok(frames)
    }
}

/// Stroke the frames resulting from a node.
#[derive(Debug, Hash)]
struct StrokeNode {
    /// How to stroke the frames resulting from the `child`.
    stroke: Stroke,
    /// The node to stroke.
    child: LayoutNode,
}

impl Layout for StrokeNode {
    fn layout(
        &self,
        vm: &mut Vm,
        regions: &Regions,
        styles: StyleChain,
    ) -> TypResult<Vec<Constrained<Arc<Frame>>>> {
        let mut frames = self.child.layout(vm, regions, styles)?;
        for Constrained { item: frame, .. } in &mut frames {
            let shape = Shape::stroked(Geometry::Rect(frame.size), self.stroke);
            Arc::make_mut(frame).prepend(Point::zero(), Element::Shape(shape));
        }
        Ok(frames)
    }
}