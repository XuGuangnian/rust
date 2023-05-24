use rustc_hir::def_id::LocalDefId;
use rustc_hir::intravisit::{self, Visitor};
use rustc_hir::{self as hir, Expr, ImplItem, Item, Node, TraitItem};
use rustc_middle::hir::nested_filter;
use rustc_middle::ty::{self, Ty, TyCtxt, TypeVisitableExt};
use rustc_span::DUMMY_SP;

use crate::errors::UnconstrainedOpaqueType;

/// Checks "defining uses" of opaque `impl Trait` types to ensure that they meet the restrictions
/// laid for "higher-order pattern unification".
/// This ensures that inference is tractable.
/// In particular, definitions of opaque types can only use other generics as arguments,
/// and they cannot repeat an argument. Example:
///
/// ```ignore (illustrative)
/// type Foo<A, B> = impl Bar<A, B>;
///
/// // Okay -- `Foo` is applied to two distinct, generic types.
/// fn a<T, U>() -> Foo<T, U> { .. }
///
/// // Not okay -- `Foo` is applied to `T` twice.
/// fn b<T>() -> Foo<T, T> { .. }
///
/// // Not okay -- `Foo` is applied to a non-generic type.
/// fn b<T>() -> Foo<T, u32> { .. }
/// ```
#[instrument(skip(tcx), level = "debug")]
pub(super) fn find_opaque_ty_constraints_for_tait(tcx: TyCtxt<'_>, def_id: LocalDefId) -> Ty<'_> {
    let hir_id = tcx.hir().local_def_id_to_hir_id(def_id);
    let scope = tcx.hir().get_defining_scope(hir_id);
    let mut locator = TaitConstraintLocator { def_id, tcx, found: None, typeck_types: vec![] };

    debug!(?scope);

    if scope == hir::CRATE_HIR_ID {
        tcx.hir().walk_toplevel_module(&mut locator);
    } else {
        trace!("scope={:#?}", tcx.hir().get(scope));
        match tcx.hir().get(scope) {
            // We explicitly call `visit_*` methods, instead of using `intravisit::walk_*` methods
            // This allows our visitor to process the defining item itself, causing
            // it to pick up any 'sibling' defining uses.
            //
            // For example, this code:
            // ```
            // fn foo() {
            //     type Blah = impl Debug;
            //     let my_closure = || -> Blah { true };
            // }
            // ```
            //
            // requires us to explicitly process `foo()` in order
            // to notice the defining usage of `Blah`.
            Node::Item(it) => locator.visit_item(it),
            Node::ImplItem(it) => locator.visit_impl_item(it),
            Node::TraitItem(it) => locator.visit_trait_item(it),
            other => bug!("{:?} is not a valid scope for an opaque type item", other),
        }
    }

    let Some(hidden) = locator.found else {
        let reported = tcx.sess.emit_err(UnconstrainedOpaqueType {
            span: tcx.def_span(def_id),
            name: tcx.item_name(tcx.local_parent(def_id).to_def_id()),
            what: match tcx.hir().get(scope) {
                _ if scope == hir::CRATE_HIR_ID => "module",
                Node::Item(hir::Item { kind: hir::ItemKind::Mod(_), .. }) => "module",
                Node::Item(hir::Item { kind: hir::ItemKind::Impl(_), .. }) => "impl",
                _ => "item",
            },
        });
        return tcx.ty_error(reported);
    };

    // Only check against typeck if we didn't already error
    if !hidden.ty.references_error() {
        for concrete_type in locator.typeck_types {
            if concrete_type.ty != tcx.erase_regions(hidden.ty)
                && !(concrete_type, hidden).references_error()
            {
                hidden.report_mismatch(&concrete_type, def_id, tcx).emit();
            }
        }
    }

    hidden.ty
}

struct TaitConstraintLocator<'tcx> {
    tcx: TyCtxt<'tcx>,

    /// def_id of the opaque type whose defining uses are being checked
    def_id: LocalDefId,

    /// as we walk the defining uses, we are checking that all of them
    /// define the same hidden type. This variable is set to `Some`
    /// with the first type that we find, and then later types are
    /// checked against it (we also carry the span of that first
    /// type).
    found: Option<ty::OpaqueHiddenType<'tcx>>,

    /// In the presence of dead code, typeck may figure out a hidden type
    /// while borrowck will not. We collect these cases here and check at
    /// the end that we actually found a type that matches (modulo regions).
    typeck_types: Vec<ty::OpaqueHiddenType<'tcx>>,
}

impl TaitConstraintLocator<'_> {
    #[instrument(skip(self), level = "debug")]
    fn check(&mut self, item_def_id: LocalDefId) {
        // Don't try to check items that cannot possibly constrain the type.
        if !self.tcx.has_typeck_results(item_def_id) {
            debug!("no constraint: no typeck results");
            return;
        }
        // Calling `mir_borrowck` can lead to cycle errors through
        // const-checking, avoid calling it if we don't have to.
        // ```rust
        // type Foo = impl Fn() -> usize; // when computing type for this
        // const fn bar() -> Foo {
        //     || 0usize
        // }
        // const BAZR: Foo = bar(); // we would mir-borrowck this, causing cycles
        // // because we again need to reveal `Foo` so we can check whether the
        // // constant does not contain interior mutability.
        // ```
        let tables = self.tcx.typeck(item_def_id);
        if let Some(guar) = tables.tainted_by_errors {
            self.found = Some(ty::OpaqueHiddenType { span: DUMMY_SP, ty: self.tcx.ty_error(guar) });
            return;
        }
        let Some(&typeck_hidden_ty) = tables.concrete_opaque_types.get(&self.def_id) else {
            debug!("no constraints in typeck results");
            return;
        };
        if self.typeck_types.iter().all(|prev| prev.ty != typeck_hidden_ty.ty) {
            self.typeck_types.push(typeck_hidden_ty);
        }

        // Use borrowck to get the type with unerased regions.
        let concrete_opaque_types = &self.tcx.mir_borrowck(item_def_id).concrete_opaque_types;
        debug!(?concrete_opaque_types);
        if let Some(&concrete_type) = concrete_opaque_types.get(&self.def_id) {
            debug!(?concrete_type, "found constraint");
            if let Some(prev) = &mut self.found {
                if concrete_type.ty != prev.ty && !(concrete_type, prev.ty).references_error() {
                    let guar = prev.report_mismatch(&concrete_type, self.def_id, self.tcx).emit();
                    prev.ty = self.tcx.ty_error(guar);
                }
            } else {
                self.found = Some(concrete_type);
            }
        }
    }
}

impl<'tcx> intravisit::Visitor<'tcx> for TaitConstraintLocator<'tcx> {
    type NestedFilter = nested_filter::All;

    fn nested_visit_map(&mut self) -> Self::Map {
        self.tcx.hir()
    }
    fn visit_expr(&mut self, ex: &'tcx Expr<'tcx>) {
        if let hir::ExprKind::Closure(closure) = ex.kind {
            self.check(closure.def_id);
        }
        intravisit::walk_expr(self, ex);
    }
    fn visit_item(&mut self, it: &'tcx Item<'tcx>) {
        trace!(?it.owner_id);
        // The opaque type itself or its children are not within its reveal scope.
        if it.owner_id.def_id != self.def_id {
            self.check(it.owner_id.def_id);
            intravisit::walk_item(self, it);
        }
    }
    fn visit_impl_item(&mut self, it: &'tcx ImplItem<'tcx>) {
        trace!(?it.owner_id);
        // The opaque type itself or its children are not within its reveal scope.
        if it.owner_id.def_id != self.def_id {
            self.check(it.owner_id.def_id);
            intravisit::walk_impl_item(self, it);
        }
    }
    fn visit_trait_item(&mut self, it: &'tcx TraitItem<'tcx>) {
        trace!(?it.owner_id);
        self.check(it.owner_id.def_id);
        intravisit::walk_trait_item(self, it);
    }
}

pub(super) fn find_opaque_ty_constraints_for_rpit(
    tcx: TyCtxt<'_>,
    def_id: LocalDefId,
    owner_def_id: LocalDefId,
) -> Ty<'_> {
    let concrete = tcx.mir_borrowck(owner_def_id).concrete_opaque_types.get(&def_id).copied();

    if let Some(concrete) = concrete {
        let scope = tcx.hir().local_def_id_to_hir_id(owner_def_id);
        debug!(?scope);
        let mut locator = RpitConstraintChecker { def_id, tcx, found: concrete };

        match tcx.hir().get(scope) {
            Node::Item(it) => intravisit::walk_item(&mut locator, it),
            Node::ImplItem(it) => intravisit::walk_impl_item(&mut locator, it),
            Node::TraitItem(it) => intravisit::walk_trait_item(&mut locator, it),
            other => bug!("{:?} is not a valid scope for an opaque type item", other),
        }
    }

    concrete.map(|concrete| concrete.ty).unwrap_or_else(|| {
        let table = tcx.typeck(owner_def_id);
        if let Some(guar) = table.tainted_by_errors {
            // Some error in the
            // owner fn prevented us from populating
            // the `concrete_opaque_types` table.
            tcx.ty_error(guar)
        } else {
            table.concrete_opaque_types.get(&def_id).map(|ty| ty.ty).unwrap_or_else(|| {
                // We failed to resolve the opaque type or it
                // resolves to itself. We interpret this as the
                // no values of the hidden type ever being constructed,
                // so we can just make the hidden type be `!`.
                // For backwards compatibility reasons, we fall back to
                // `()` until we the diverging default is changed.
                tcx.mk_diverging_default()
            })
        }
    })
}

struct RpitConstraintChecker<'tcx> {
    tcx: TyCtxt<'tcx>,

    /// def_id of the opaque type whose defining uses are being checked
    def_id: LocalDefId,

    found: ty::OpaqueHiddenType<'tcx>,
}

impl RpitConstraintChecker<'_> {
    #[instrument(skip(self), level = "debug")]
    fn check(&self, def_id: LocalDefId) {
        // Use borrowck to get the type with unerased regions.
        let concrete_opaque_types = &self.tcx.mir_borrowck(def_id).concrete_opaque_types;
        debug!(?concrete_opaque_types);
        for (&def_id, &concrete_type) in concrete_opaque_types {
            if def_id != self.def_id {
                // Ignore constraints for other opaque types.
                continue;
            }

            debug!(?concrete_type, "found constraint");

            if concrete_type.ty != self.found.ty && !(concrete_type, self.found).references_error()
            {
                self.found.report_mismatch(&concrete_type, self.def_id, self.tcx).emit();
            }
        }
    }
}

impl<'tcx> intravisit::Visitor<'tcx> for RpitConstraintChecker<'tcx> {
    type NestedFilter = nested_filter::OnlyBodies;

    fn nested_visit_map(&mut self) -> Self::Map {
        self.tcx.hir()
    }
    fn visit_expr(&mut self, ex: &'tcx Expr<'tcx>) {
        if let hir::ExprKind::Closure(closure) = ex.kind {
            self.check(closure.def_id);
        }
        intravisit::walk_expr(self, ex);
    }
    fn visit_item(&mut self, it: &'tcx Item<'tcx>) {
        trace!(?it.owner_id);
        // The opaque type itself or its children are not within its reveal scope.
        if it.owner_id.def_id != self.def_id {
            self.check(it.owner_id.def_id);
            intravisit::walk_item(self, it);
        }
    }
    fn visit_impl_item(&mut self, it: &'tcx ImplItem<'tcx>) {
        trace!(?it.owner_id);
        // The opaque type itself or its children are not within its reveal scope.
        if it.owner_id.def_id != self.def_id {
            self.check(it.owner_id.def_id);
            intravisit::walk_impl_item(self, it);
        }
    }
    fn visit_trait_item(&mut self, it: &'tcx TraitItem<'tcx>) {
        trace!(?it.owner_id);
        self.check(it.owner_id.def_id);
        intravisit::walk_trait_item(self, it);
    }
}
