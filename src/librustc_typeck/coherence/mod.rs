// Copyright 2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// Coherence phase
//
// The job of the coherence phase of typechecking is to ensure that
// each trait has at most one implementation for each type. This is
// done by the orphan and overlap modules. Then we build up various
// mappings. That mapping code resides here.

use hir::def_id::{DefId, LOCAL_CRATE};
use rustc::traits;
use rustc::ty::{self, TyCtxt, TypeFoldable};
use rustc::ty::maps::Providers;

use syntax::ast;

mod builtin;
mod inherent_impls;
mod inherent_impls_overlap;
mod orphan;
mod unsafety;

fn check_impl<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>, node_id: ast::NodeId) {
    let impl_def_id = tcx.hir.local_def_id(node_id);

    // If there are no traits, then this implementation must have a
    // base type.

    if let Some(trait_ref) = tcx.impl_trait_ref(impl_def_id) {
        debug!("(checking implementation) adding impl for trait '{:?}', item '{}'",
                trait_ref,
                tcx.item_path_str(impl_def_id));

        // Skip impls where one of the self type is an error type.
        // This occurs with e.g. resolve failures (#30589).
        if trait_ref.references_error() {
            return;
        }

        enforce_trait_manually_implementable(tcx, impl_def_id, trait_ref.def_id);
    }
}

fn enforce_trait_manually_implementable(tcx: TyCtxt, impl_def_id: DefId, trait_def_id: DefId) {
    let did = Some(trait_def_id);
    let li = tcx.lang_items();
    let span = tcx.sess.codemap().def_span(tcx.span_of_impl(impl_def_id).unwrap());

    // Disallow *all* explicit impls of `Sized` and `Unsize` for now.
    if did == li.sized_trait() {
        struct_span_err!(tcx.sess,
                         span,
                         E0322,
                         "explicit impls for the `Sized` trait are not permitted")
            .span_label(span, "impl of 'Sized' not allowed")
            .emit();
        return;
    }

    if did == li.unsize_trait() {
        struct_span_err!(tcx.sess,
                         span,
                         E0328,
                         "explicit impls for the `Unsize` trait are not permitted")
            .span_label(span, "impl of `Unsize` not allowed")
            .emit();
        return;
    }

    if tcx.features().unboxed_closures {
        // the feature gate allows all Fn traits
        return;
    }

    let trait_name = if did == li.fn_trait() {
        "Fn"
    } else if did == li.fn_mut_trait() {
        "FnMut"
    } else if did == li.fn_once_trait() {
        "FnOnce"
    } else {
        return; // everything OK
    };
    struct_span_err!(tcx.sess,
                     span,
                     E0183,
                     "manual implementations of `{}` are experimental",
                     trait_name)
        .span_label(span, format!("manual implementations of `{}` are experimental", trait_name))
        .help("add `#![feature(unboxed_closures)]` to the crate attributes to enable")
        .emit();
}

pub fn provide(providers: &mut Providers) {
    use self::builtin::coerce_unsized_info;
    use self::inherent_impls::{crate_inherent_impls, inherent_impls};
    use self::inherent_impls_overlap::crate_inherent_impls_overlap_check;

    *providers = Providers {
        coherent_trait,
        crate_inherent_impls,
        inherent_impls,
        crate_inherent_impls_overlap_check,
        coerce_unsized_info,
        ..*providers
    };
}

fn coherent_trait<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>, def_id: DefId) {
    let impls = tcx.hir.trait_impls(def_id);
    for &impl_id in impls {
        check_impl(tcx, impl_id);
    }
    for &impl_id in impls {
        check_impl_overlap(tcx, impl_id);
    }
    builtin::check_trait(tcx, def_id);
}

pub fn check_coherence<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>) {
    for &trait_def_id in tcx.hir.krate().trait_impls.keys() {
        ty::maps::queries::coherent_trait::ensure(tcx, trait_def_id);
    }

    unsafety::check(tcx);
    orphan::check(tcx);

    // these queries are executed for side-effects (error reporting):
    ty::maps::queries::crate_inherent_impls::ensure(tcx, LOCAL_CRATE);
    ty::maps::queries::crate_inherent_impls_overlap_check::ensure(tcx, LOCAL_CRATE);
}

/// Overlap: No two impls for the same trait are implemented for the
/// same type. Likewise, no two inherent impls for a given type
/// constructor provide a method with the same name.
fn check_impl_overlap<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>, node_id: ast::NodeId) {
    let impl_def_id = tcx.hir.local_def_id(node_id);
    let trait_ref = tcx.impl_trait_ref(impl_def_id).unwrap();
    let trait_def_id = trait_ref.def_id;

    if trait_ref.references_error() {
        debug!("coherence: skipping impl {:?} with error {:?}",
               impl_def_id, trait_ref);
        return
    }

    // Trigger building the specialization graph for the trait of this impl.
    // This will detect any overlap errors.
    tcx.specialization_graph_of(trait_def_id);

    // check for overlap with the automatic `impl Trait for Trait`
    if let ty::TyDynamic(ref data, ..) = trait_ref.self_ty().sty {
        // This is something like impl Trait1 for Trait2. Illegal
        // if Trait1 is a supertrait of Trait2 or Trait2 is not object safe.

        if data.principal().map_or(true, |p| !tcx.is_object_safe(p.def_id())) {
            // This is an error, but it will be reported by wfcheck.  Ignore it here.
            // This is tested by `coherence-impl-trait-for-trait-object-safe.rs`.
        } else {
            let mut supertrait_def_ids =
                traits::supertrait_def_ids(tcx,
                                           data.principal().unwrap().def_id());
            if supertrait_def_ids.any(|d| d == trait_def_id) {
                let sp = tcx.sess.codemap().def_span(tcx.span_of_impl(impl_def_id).unwrap());
                struct_span_err!(tcx.sess,
                                 sp,
                                 E0371,
                                 "the object type `{}` automatically implements the trait `{}`",
                                 trait_ref.self_ty(),
                                 tcx.item_path_str(trait_def_id))
                    .span_label(sp, format!("`{}` automatically implements trait `{}`",
                                            trait_ref.self_ty(),
                                            tcx.item_path_str(trait_def_id)))
                    .emit();
            }
        }
    }
}
