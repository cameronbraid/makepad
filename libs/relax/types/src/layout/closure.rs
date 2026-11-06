use crate::expr_use::{BorrowKind, Projection};
use crate::tcx::Tcx;
use crate::typeck::Body;
use crate::types::{TyId, TyKind};

// EXCEPTION(N4): root-slot closure storage until per-place capture codegen
#[derive(Clone, Default, PartialEq)]
pub struct ClosureStorage {
    pub fields: Vec<TyId>,
    pub roots: Vec<u32>,
    /// A shared reference field reached through a borrowed root can outlive that root
    /// (for example an iterator returned by `&self`). Store the reference itself,
    /// and resolve body places from this prefix instead of retaining `&self`.
    pub shared_prefixes: Vec<Option<Vec<(Projection, TyId)>>>,
    pub by_value: Vec<bool>,
    pub owned_paths: Vec<Option<Vec<Vec<u32>>>>,
    /// Per root: its field is a `&mut T` the closure only reads through (every capture is a
    /// shared borrow of a place behind it). rustc's capture there is a `&` of that place, which
    /// is Copy: a clone of the closure copies the pointer (the field keeps the `&mut T` value
    /// for the root-slot ABI above, and a `&mut` is never Clone itself).
    pub shared_reborrow: Vec<bool>,
    pub drop_flags: bool,
}

impl ClosureStorage {
    pub fn new(tcx: &Tcx, body: &Body, ce: u32) -> Self {
        Self::with_types(tcx, body, ce, |ty| ty)
    }

    /// The parent may be the shared typeck result. Normalize capture reads without
    /// copying its tables (rustc_codegen_ssa::mir::FunctionCx::monomorphize).
    pub fn for_codegen(prog: &crate::program::Program, tcx: &Tcx, body: &Body, ce: u32) -> Self {
        Self::with_types(tcx, body, ce, |ty| tcx.normalize_erasing_regions(
            prog, crate::tcx::TypingEnv::fully_monomorphized(prog), ty))
    }

    fn with_types(tcx: &Tcx, body: &Body, ce: u32, ty: impl Fn(TyId) -> TyId) -> Self {
        let captures = &body.min_captures[&ce].captures;
        let roots: Vec<_> = body
            .closure_locals
            .get(&ce)
            .into_iter()
            .flatten()
            .copied()
            .filter(|root| captures.iter().any(|c| c.place.root == *root))
            .collect();
        let mut by_value = Vec::new();
        let mut shared_prefixes = Vec::new();
        let mut shared_reborrow = Vec::new();
        let mut fields = Vec::new();
        let mut upvars = Vec::new();
        for &root in &roots {
            let cs: Vec<_> = captures.iter().filter(|c| c.place.root == root).collect();
            let root_ty = ty(body.locals[root as usize].ty);
            // Only cross builtin reference/field projections. Every use must continue through the
            // same reference field, so copying that pointer preserves shared identity
            // without copying the containing value or extending a temporary borrow.
            let shared_prefix = cs.first().and_then(|first| {
                first.place.projections.iter().enumerate().rev().find_map(|(i, (p, _))| {
                    if *p != Projection::Deref || i == 0
                        || !matches!(first.place.projections[i - 1].0, Projection::Field(..))
                        || !matches!(tcx.tys.kind(ty(first.place.before(i))), TyKind::Ref(false, ..))
                        || !first.place.projections[..i].iter().enumerate().all(|(j, (p, _))| match p {
                            Projection::Field(..) => true,
                            Projection::Deref => matches!(tcx.tys.kind(ty(first.place.before(j))), TyKind::Ref(..)),
                            _ => false,
                        })
                        || !cs.iter().all(|c| c.place.projections.starts_with(&first.place.projections[..=i]))
                    { return None; }
                    Some(first.place.projections[..i].iter().map(|(p, t)| (*p, ty(*t))).collect::<Vec<_>>())
                })
            });
            let ty = shared_prefix.as_ref().and_then(|p| p.last().map(|p| p.1)).unwrap_or(root_ty);
            let projected_shared = shared_prefix.is_some();
            shared_prefixes.push(shared_prefix);
            // A captured *ref is already a pointer to its referent. The old
            // ABI accesses the root, so store that reference value, not &ref.
            let deref_shared = matches!(tcx.tys.kind(ty), TyKind::Ref(..))
                && cs.iter().all(|c| {
                    c.place
                        .projections
                        .first()
                        .is_some_and(|p| p.0 == Projection::Deref)
                });
            let value = projected_shared || deref_shared || cs.iter().any(|c| c.info.kind == crate::upvar::CaptureKind::ByValue);
            let mutable = cs.iter().any(|c| {
                matches!(
                    c.info.kind,
                    crate::upvar::CaptureKind::ByRef(BorrowKind::Mut | BorrowKind::UniqueImm)
                )
            });
            by_value.push(value);
            shared_reborrow.push(deref_shared && matches!(tcx.tys.kind(ty), TyKind::Ref(true, ..))
                && cs.iter().all(|c| c.info.kind == crate::upvar::CaptureKind::ByRef(BorrowKind::Imm)));
            upvars.push(if value {
                ty
            } else {
                tcx.tys.intern(TyKind::Ref(mutable, ty, crate::types::RegionId::ERASED))
            });
            let mut fs = Vec::new();
            let precise = value
                && !deref_shared && !projected_shared
                && cs.iter().filter(|c| c.info.kind == crate::upvar::CaptureKind::ByValue).all(|c| {
                    let path: Vec<_> = c.place.projections.iter().map_while(|(p, _)| {
                        if let Projection::Field(0, field) = p { Some(*field) } else { None }
                    }).collect();
                    if path.is_empty() { false } else { fs.push(path); true }
                });
            fs.sort_unstable();
            fs.dedup();
            // Truncation at a dereference may leave a parent and its child.
            // The parent already owns the whole subtree.
            let mut minimal: Vec<Vec<u32>> = Vec::new();
            for path in fs {
                if !minimal.iter().any(|parent| path.starts_with(parent)) { minimal.push(path); }
            }
            fields.push(precise.then_some(minimal));
        }
        let result = &body.min_captures[&ce];
        let drop_flags = if result.suspension_points.is_some() {
            by_value.iter().any(|v| *v)
        } else { result.kind == crate::upvar::ClosureKind::FnOnce };
        if drop_flags { for _ in &roots { upvars.push(tcx.tys.u8_); } }
        if let Some(points) = result.suspension_points {
            let states = points as u64 + 3;
            upvars.push(tcx.tys.prim(if states <= 1 << 8 { crate::program::Prim::U8 }
                else if states <= 1 << 16 { crate::program::Prim::U16 } else { crate::program::Prim::U32 }));
        }
        Self { fields: upvars, roots, shared_prefixes, by_value, owned_paths: fields, shared_reborrow, drop_flags }
    }
}
