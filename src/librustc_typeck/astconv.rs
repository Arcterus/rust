// Copyright 2012-2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Conversion from AST representation of types to the ty.rs
//! representation.  The main routine here is `ast_ty_to_ty()`: each use
//! is parameterized by an instance of `AstConv` and a `RegionScope`.
//!
//! The parameterization of `ast_ty_to_ty()` is because it behaves
//! somewhat differently during the collect and check phases,
//! particularly with respect to looking up the types of top-level
//! items.  In the collect phase, the crate context is used as the
//! `AstConv` instance; in this phase, the `get_item_ty()` function
//! triggers a recursive call to `ty_of_item()`  (note that
//! `ast_ty_to_ty()` will detect recursive types and report an error).
//! In the check phase, when the FnCtxt is used as the `AstConv`,
//! `get_item_ty()` just looks up the item type in `tcx.tcache`.
//!
//! The `RegionScope` trait controls what happens when the user does
//! not specify a region in some location where a region is required
//! (e.g., if the user writes `&Foo` as a type rather than `&'a Foo`).
//! See the `rscope` module for more details.
//!
//! Unlike the `AstConv` trait, the region scope can change as we descend
//! the type.  This is to accommodate the fact that (a) fn types are binding
//! scopes and (b) the default region may change.  To understand case (a),
//! consider something like:
//!
//!   type foo = { x: &a.int, y: |&a.int| }
//!
//! The type of `x` is an error because there is no region `a` in scope.
//! In the type of `y`, however, region `a` is considered a bound region
//! as it does not already appear in scope.
//!
//! Case (b) says that if you have a type:
//!   type foo<'a> = ...;
//!   type bar = fn(&foo, &a.foo)
//! The fully expanded version of type bar is:
//!   type bar = fn(&'foo &, &a.foo<'a>)
//! Note that the self region for the `foo` defaulted to `&` in the first
//! case but `&a` in the second.  Basically, defaults that appear inside
//! an rptr (`&r.T`) use the region `r` that appears in the rptr.

use middle::astconv_util::{ast_ty_to_prim_ty, check_path_args, NO_TPS, NO_REGIONS};
use middle::const_eval;
use middle::def;
use middle::resolve_lifetime as rl;
use middle::subst::{FnSpace, TypeSpace, AssocSpace, SelfSpace, Subst, Substs};
use middle::subst::{VecPerParamSpace};
use middle::ty::{mod, Ty};
use rscope::{mod, UnelidableRscope, RegionScope, SpecificRscope,
             ShiftedRscope, BindingRscope};
use TypeAndSubsts;
use util::common::ErrorReported;
use util::nodemap::DefIdMap;
use util::ppaux::{mod, Repr, UserString};

use std::rc::Rc;
use std::iter::AdditiveIterator;
use syntax::{abi, ast, ast_util};
use syntax::codemap::Span;
use syntax::parse::token;
use syntax::print::pprust;

pub trait AstConv<'tcx> {
    fn tcx<'a>(&'a self) -> &'a ty::ctxt<'tcx>;
    fn get_item_ty(&self, id: ast::DefId) -> ty::Polytype<'tcx>;
    fn get_trait_def(&self, id: ast::DefId) -> Rc<ty::TraitDef<'tcx>>;

    /// What type should we use when a type is omitted?
    fn ty_infer(&self, span: Span) -> Ty<'tcx>;

    /// Returns true if associated types from the given trait and type are
    /// allowed to be used here and false otherwise.
    fn associated_types_of_trait_are_valid(&self,
                                           ty: Ty<'tcx>,
                                           trait_id: ast::DefId)
                                           -> bool;

    /// Returns the binding of the given associated type for some type.
    fn associated_type_binding(&self,
                               span: Span,
                               ty: Option<Ty<'tcx>>,
                               trait_id: ast::DefId,
                               associated_type_id: ast::DefId)
                               -> Ty<'tcx>;
}

pub fn ast_region_to_region(tcx: &ty::ctxt, lifetime: &ast::Lifetime)
                            -> ty::Region {
    let r = match tcx.named_region_map.get(&lifetime.id) {
        None => {
            // should have been recorded by the `resolve_lifetime` pass
            tcx.sess.span_bug(lifetime.span, "unresolved lifetime");
        }

        Some(&rl::DefStaticRegion) => {
            ty::ReStatic
        }

        Some(&rl::DefLateBoundRegion(debruijn, id)) => {
            ty::ReLateBound(debruijn, ty::BrNamed(ast_util::local_def(id), lifetime.name))
        }

        Some(&rl::DefEarlyBoundRegion(space, index, id)) => {
            ty::ReEarlyBound(id, space, index, lifetime.name)
        }

        Some(&rl::DefFreeRegion(scope, id)) => {
            ty::ReFree(ty::FreeRegion {
                    scope: scope,
                    bound_region: ty::BrNamed(ast_util::local_def(id),
                                              lifetime.name)
                })
        }
    };

    debug!("ast_region_to_region(lifetime={} id={}) yields {}",
           lifetime.repr(tcx),
           lifetime.id,
           r.repr(tcx));

    r
}

pub fn opt_ast_region_to_region<'tcx, AC: AstConv<'tcx>, RS: RegionScope>(
    this: &AC,
    rscope: &RS,
    default_span: Span,
    opt_lifetime: &Option<ast::Lifetime>) -> ty::Region
{
    let r = match *opt_lifetime {
        Some(ref lifetime) => {
            ast_region_to_region(this.tcx(), lifetime)
        }

        None => {
            match rscope.anon_regions(default_span, 1) {
                Err(v) => {
                    debug!("optional region in illegal location");
                    span_err!(this.tcx().sess, default_span, E0106,
                        "missing lifetime specifier");
                    match v {
                        Some(v) => {
                            let mut m = String::new();
                            let len = v.len();
                            for (i, (name, n)) in v.into_iter().enumerate() {
                                m.push_str(if n == 1 {
                                    format!("`{}`", name)
                                } else {
                                    format!("one of `{}`'s {} elided lifetimes", name, n)
                                }.as_slice());

                                if len == 2 && i == 0 {
                                    m.push_str(" or ");
                                } else if i == len - 2 {
                                    m.push_str(", or ");
                                } else if i != len - 1 {
                                    m.push_str(", ");
                                }
                            }
                            if len == 1 {
                                span_help!(this.tcx().sess, default_span,
                                    "this function's return type contains a borrowed value, but \
                                     the signature does not say which {} it is borrowed from",
                                    m);
                            } else if len == 0 {
                                span_help!(this.tcx().sess, default_span,
                                    "this function's return type contains a borrowed value, but \
                                     there is no value for it to be borrowed from");
                                span_help!(this.tcx().sess, default_span,
                                    "consider giving it a 'static lifetime");
                            } else {
                                span_help!(this.tcx().sess, default_span,
                                    "this function's return type contains a borrowed value, but \
                                     the signature does not say whether it is borrowed from {}",
                                    m);
                            }
                        }
                        None => {},
                    }
                    ty::ReStatic
                }

                Ok(rs) => rs[0],
            }
        }
    };

    debug!("opt_ast_region_to_region(opt_lifetime={}) yields {}",
            opt_lifetime.repr(this.tcx()),
            r.repr(this.tcx()));

    r
}

/// Given a path `path` that refers to an item `I` with the declared generics `decl_generics`,
/// returns an appropriate set of substitutions for this particular reference to `I`.
fn ast_path_substs_for_ty<'tcx,AC,RS>(
    this: &AC,
    rscope: &RS,
    decl_def_id: ast::DefId,
    decl_generics: &ty::Generics<'tcx>,
    self_ty: Option<Ty<'tcx>>,
    path: &ast::Path)
    -> Substs<'tcx>
    where AC: AstConv<'tcx>, RS: RegionScope
{
    let tcx = this.tcx();

    // ast_path_substs() is only called to convert paths that are
    // known to refer to traits, types, or structs. In these cases,
    // all type parameters defined for the item being referenced will
    // be in the TypeSpace or SelfSpace.
    //
    // Note: in the case of traits, the self parameter is also
    // defined, but we don't currently create a `type_param_def` for
    // `Self` because it is implicit.
    assert!(decl_generics.regions.all(|d| d.space == TypeSpace));
    assert!(decl_generics.types.all(|d| d.space != FnSpace));

    let (regions, types) = match path.segments.last().unwrap().parameters {
        ast::AngleBracketedParameters(ref data) => {
            convert_angle_bracketed_parameters(this, rscope, data)
        }
        ast::ParenthesizedParameters(ref data) => {
            span_err!(tcx.sess, path.span, E0169,
                      "parenthesized parameters may only be used with a trait");
            (Vec::new(), convert_parenthesized_parameters(this, data))
        }
    };

    create_substs_for_ast_path(this, rscope, path.span, decl_def_id,
                               decl_generics, self_ty, types, regions)
}

fn create_substs_for_ast_path<'tcx,AC,RS>(
    this: &AC,
    rscope: &RS,
    span: Span,
    decl_def_id: ast::DefId,
    decl_generics: &ty::Generics<'tcx>,
    self_ty: Option<Ty<'tcx>>,
    types: Vec<Ty<'tcx>>,
    regions: Vec<ty::Region>)
    -> Substs<'tcx>
    where AC: AstConv<'tcx>, RS: RegionScope
{
    let tcx = this.tcx();

    // If the type is parameterized by the this region, then replace this
    // region with the current anon region binding (in other words,
    // whatever & would get replaced with).
    let expected_num_region_params = decl_generics.regions.len(TypeSpace);
    let supplied_num_region_params = regions.len();
    let regions = if expected_num_region_params == supplied_num_region_params {
        regions
    } else {
        let anon_regions =
            rscope.anon_regions(span, expected_num_region_params);

        if supplied_num_region_params != 0 || anon_regions.is_err() {
            span_err!(tcx.sess, span, E0107,
                      "wrong number of lifetime parameters: expected {}, found {}",
                      expected_num_region_params, supplied_num_region_params);
        }

        match anon_regions {
            Ok(v) => v.into_iter().collect(),
            Err(_) => Vec::from_fn(expected_num_region_params,
                                   |_| ty::ReStatic) // hokey
        }
    };

    // Convert the type parameters supplied by the user.
    let ty_param_defs = decl_generics.types.get_slice(TypeSpace);
    let supplied_ty_param_count = types.len();
    let formal_ty_param_count =
        ty_param_defs.iter()
        .take_while(|x| !ty::is_associated_type(tcx, x.def_id))
        .count();
    let required_ty_param_count =
        ty_param_defs.iter()
        .take_while(|x| {
            x.default.is_none() &&
                !ty::is_associated_type(tcx, x.def_id)
        })
        .count();
    if supplied_ty_param_count < required_ty_param_count {
        let expected = if required_ty_param_count < formal_ty_param_count {
            "expected at least"
        } else {
            "expected"
        };
        this.tcx().sess.span_fatal(span,
                                   format!("wrong number of type arguments: {} {}, found {}",
                                           expected,
                                           required_ty_param_count,
                                           supplied_ty_param_count).as_slice());
    } else if supplied_ty_param_count > formal_ty_param_count {
        let expected = if required_ty_param_count < formal_ty_param_count {
            "expected at most"
        } else {
            "expected"
        };
        this.tcx().sess.span_fatal(span,
                                   format!("wrong number of type arguments: {} {}, found {}",
                                           expected,
                                           formal_ty_param_count,
                                           supplied_ty_param_count).as_slice());
    }

    if supplied_ty_param_count > required_ty_param_count
        && !this.tcx().sess.features.borrow().default_type_params {
        span_err!(this.tcx().sess, span, E0108,
            "default type parameters are experimental and possibly buggy");
        span_help!(this.tcx().sess, span,
            "add #![feature(default_type_params)] to the crate attributes to enable");
    }

    let mut substs = Substs::new_type(types, regions);

    match self_ty {
        None => {
            // If no self-type is provided, it's still possible that
            // one was declared, because this could be an object type.
        }
        Some(ty) => {
            // If a self-type is provided, one should have been
            // "declared" (in other words, this should be a
            // trait-ref).
            assert!(decl_generics.types.get_self().is_some());
            substs.types.push(SelfSpace, ty);
        }
    }

    for param in ty_param_defs[supplied_ty_param_count..].iter() {
        match param.default {
            Some(default) => {
                // This is a default type parameter.
                let default = default.subst_spanned(tcx,
                                                    &substs,
                                                    Some(span));
                substs.types.push(TypeSpace, default);
            }
            None => {
                tcx.sess.span_bug(span, "extra parameter without default");
            }
        }
    }

    for param in decl_generics.types.get_slice(AssocSpace).iter() {
        substs.types.push(
            AssocSpace,
            this.associated_type_binding(span,
                                         self_ty,
                                         decl_def_id,
                                         param.def_id));
    }

    return substs;
}

fn convert_angle_bracketed_parameters<'tcx, AC, RS>(this: &AC,
                                                    rscope: &RS,
                                                    data: &ast::AngleBracketedParameterData)
                                                    -> (Vec<ty::Region>, Vec<Ty<'tcx>>)
    where AC: AstConv<'tcx>, RS: RegionScope
{
    let regions: Vec<_> =
        data.lifetimes.iter()
        .map(|l| ast_region_to_region(this.tcx(), l))
        .collect();

    let types: Vec<_> =
        data.types.iter()
        .map(|t| ast_ty_to_ty(this, rscope, &**t))
        .collect();

    (regions, types)
}

fn convert_parenthesized_parameters<'tcx,AC>(this: &AC,
                                             data: &ast::ParenthesizedParameterData)
                                             -> Vec<Ty<'tcx>>
    where AC: AstConv<'tcx>
{
    let binding_rscope = BindingRscope::new();

    let inputs = data.inputs.iter()
                            .map(|a_t| ast_ty_to_ty(this, &binding_rscope, &**a_t))
                            .collect();
    let input_ty = ty::mk_tup(this.tcx(), inputs);

    let output = match data.output {
        Some(ref output_ty) => ast_ty_to_ty(this, &binding_rscope, &**output_ty),
        None => ty::mk_nil(this.tcx()),
    };

    vec![input_ty, output]
}

pub fn instantiate_poly_trait_ref<'tcx,AC,RS>(
    this: &AC,
    rscope: &RS,
    ast_trait_ref: &ast::PolyTraitRef,
    self_ty: Option<Ty<'tcx>>)
    -> Rc<ty::TraitRef<'tcx>>
    where AC: AstConv<'tcx>, RS: RegionScope
{
    instantiate_trait_ref(this, rscope, &ast_trait_ref.trait_ref, self_ty)
}

/// Instantiates the path for the given trait reference, assuming that it's bound to a valid trait
/// type. Returns the def_id for the defining trait. Fails if the type is a type other than a trait
/// type.
pub fn instantiate_trait_ref<'tcx,AC,RS>(this: &AC,
                                         rscope: &RS,
                                         ast_trait_ref: &ast::TraitRef,
                                         self_ty: Option<Ty<'tcx>>)
                                         -> Rc<ty::TraitRef<'tcx>>
                                         where AC: AstConv<'tcx>,
                                               RS: RegionScope
{
    match ::lookup_def_tcx(this.tcx(),
                           ast_trait_ref.path.span,
                           ast_trait_ref.ref_id) {
        def::DefTrait(trait_def_id) => {
            let trait_ref = Rc::new(ast_path_to_trait_ref(this, rscope, trait_def_id,
                                                          self_ty, &ast_trait_ref.path));
            this.tcx().trait_refs.borrow_mut().insert(ast_trait_ref.ref_id,
                                                      trait_ref.clone());
            trait_ref
        }
        _ => {
            this.tcx().sess.span_fatal(
                ast_trait_ref.path.span,
                format!("`{}` is not a trait", ast_trait_ref.path.user_string(this.tcx()))[]);
        }
    }
}

fn ast_path_to_trait_ref<'tcx,AC,RS>(
    this: &AC,
    rscope: &RS,
    trait_def_id: ast::DefId,
    self_ty: Option<Ty<'tcx>>,
    path: &ast::Path)
    -> ty::TraitRef<'tcx>
    where AC: AstConv<'tcx>, RS: RegionScope
{
    let trait_def = this.get_trait_def(trait_def_id);

    // the trait reference introduces a binding level here, so
    // we need to shift the `rscope`. It'd be nice if we could
    // do away with this rscope stuff and work this knowledge
    // into resolve_lifetimes, as we do with non-omitted
    // lifetimes. Oh well, not there yet.
    let shifted_rscope = ShiftedRscope::new(rscope);

    let (regions, types) = match path.segments.last().unwrap().parameters {
        ast::AngleBracketedParameters(ref data) => {
            convert_angle_bracketed_parameters(this, &shifted_rscope, data)
        }
        ast::ParenthesizedParameters(ref data) => {
            (Vec::new(), convert_parenthesized_parameters(this, data))
        }
    };

    let substs = create_substs_for_ast_path(this,
                                            &shifted_rscope,
                                            path.span,
                                            trait_def_id,
                                            &trait_def.generics,
                                            self_ty,
                                            types,
                                            regions);

    ty::TraitRef::new(trait_def_id, substs)
}

pub fn ast_path_to_ty<'tcx, AC: AstConv<'tcx>, RS: RegionScope>(
    this: &AC,
    rscope: &RS,
    did: ast::DefId,
    path: &ast::Path)
    -> TypeAndSubsts<'tcx>
{
    let tcx = this.tcx();
    let ty::Polytype {
        generics,
        ty: decl_ty
    } = this.get_item_ty(did);

    let substs = ast_path_substs_for_ty(this,
                                        rscope,
                                        did,
                                        &generics,
                                        None,
                                        path);
    let ty = decl_ty.subst(tcx, &substs);
    TypeAndSubsts { substs: substs, ty: ty }
}

/// Returns the type that this AST path refers to. If the path has no type
/// parameters and the corresponding type has type parameters, fresh type
/// and/or region variables are substituted.
///
/// This is used when checking the constructor in struct literals.
pub fn ast_path_to_ty_relaxed<'tcx,AC,RS>(
    this: &AC,
    rscope: &RS,
    did: ast::DefId,
    path: &ast::Path)
    -> TypeAndSubsts<'tcx>
    where AC : AstConv<'tcx>, RS : RegionScope
{
    let tcx = this.tcx();
    let ty::Polytype {
        generics,
        ty: decl_ty
    } = this.get_item_ty(did);

    let wants_params =
        generics.has_type_params(TypeSpace) || generics.has_region_params(TypeSpace);

    let needs_defaults =
        wants_params &&
        path.segments.iter().all(|s| s.parameters.is_empty());

    let substs = if needs_defaults {
        let type_params = Vec::from_fn(generics.types.len(TypeSpace),
                                       |_| this.ty_infer(path.span));
        let region_params =
            rscope.anon_regions(path.span, generics.regions.len(TypeSpace))
                  .unwrap();
        Substs::new(VecPerParamSpace::params_from_type(type_params),
                    VecPerParamSpace::params_from_type(region_params))
    } else {
        ast_path_substs_for_ty(this, rscope, did, &generics, None, path)
    };

    let ty = decl_ty.subst(tcx, &substs);
    TypeAndSubsts {
        substs: substs,
        ty: ty,
    }
}

/// Converts the given AST type to a built-in type. A "built-in type" is, at
/// present, either a core numeric type, a string, or `Box`.
pub fn ast_ty_to_builtin_ty<'tcx, AC: AstConv<'tcx>, RS: RegionScope>(
        this: &AC,
        rscope: &RS,
        ast_ty: &ast::Ty)
        -> Option<Ty<'tcx>> {
    match ast_ty_to_prim_ty(this.tcx(), ast_ty) {
        Some(typ) => return Some(typ),
        None => {}
    }

    match ast_ty.node {
        ast::TyPath(ref path, id) => {
            let a_def = match this.tcx().def_map.borrow().get(&id) {
                None => {
                    this.tcx()
                        .sess
                        .span_bug(ast_ty.span,
                                  format!("unbound path {}",
                                          path.repr(this.tcx())).as_slice())
                }
                Some(&d) => d
            };

            // FIXME(#12938): This is a hack until we have full support for
            // DST.
            match a_def {
                def::DefTy(did, _) |
                def::DefStruct(did) if Some(did) == this.tcx().lang_items.owned_box() => {
                    let ty = ast_path_to_ty(this, rscope, did, path).ty;
                    match ty.sty {
                        ty::ty_struct(struct_def_id, ref substs) => {
                            assert_eq!(struct_def_id, did);
                            assert_eq!(substs.types.len(TypeSpace), 1);
                            let referent_ty = *substs.types.get(TypeSpace, 0);
                            Some(ty::mk_uniq(this.tcx(), referent_ty))
                        }
                        _ => {
                            this.tcx().sess.span_bug(
                                path.span,
                                format!("converting `Box` to `{}`",
                                        ty.repr(this.tcx()))[]);
                        }
                    }
                }
                _ => None
            }
        }
        _ => None
    }
}

fn ast_ty_to_trait_ref<'tcx,AC,RS>(this: &AC,
                                   rscope: &RS,
                                   ty: &ast::Ty,
                                   bounds: &[ast::TyParamBound])
                                   -> Result<ty::TraitRef<'tcx>, ErrorReported>
    where AC : AstConv<'tcx>, RS : RegionScope
{
    /*!
     * In a type like `Foo + Send`, we want to wait to collect the
     * full set of bounds before we make the object type, because we
     * need them to infer a region bound.  (For example, if we tried
     * made a type from just `Foo`, then it wouldn't be enough to
     * infer a 'static bound, and hence the user would get an error.)
     * So this function is used when we're dealing with a sum type to
     * convert the LHS. It only accepts a type that refers to a trait
     * name, and reports an error otherwise.
     */

    match ty.node {
        ast::TyPath(ref path, id) => {
            match this.tcx().def_map.borrow().get(&id) {
                Some(&def::DefTrait(trait_def_id)) => {
                    return Ok(ast_path_to_trait_ref(this,
                                                    rscope,
                                                    trait_def_id,
                                                    None,
                                                    path));
                }
                _ => {
                    span_err!(this.tcx().sess, ty.span, E0172, "expected a reference to a trait");
                    Err(ErrorReported)
                }
            }
        }
        _ => {
            span_err!(this.tcx().sess, ty.span, E0171,
                      "expected a path on the left-hand side of `+`, not `{}`",
                      pprust::ty_to_string(ty));
            match ty.node {
                ast::TyRptr(None, ref mut_ty) => {
                    span_note!(this.tcx().sess, ty.span,
                               "perhaps you meant `&{}({} +{})`? (per RFC 248)",
                               ppaux::mutability_to_string(mut_ty.mutbl),
                               pprust::ty_to_string(&*mut_ty.ty),
                               pprust::bounds_to_string(bounds));
                }

                ast::TyRptr(Some(ref lt), ref mut_ty) => {
                    span_note!(this.tcx().sess, ty.span,
                               "perhaps you meant `&{} {}({} +{})`? (per RFC 248)",
                               pprust::lifetime_to_string(lt),
                               ppaux::mutability_to_string(mut_ty.mutbl),
                               pprust::ty_to_string(&*mut_ty.ty),
                               pprust::bounds_to_string(bounds));
                }

                _ => {
                    span_note!(this.tcx().sess, ty.span,
                               "perhaps you forgot parentheses? (per RFC 248)");
                }
            }
            Err(ErrorReported)
        }
    }

}

fn trait_ref_to_object_type<'tcx,AC,RS>(this: &AC,
                                        rscope: &RS,
                                        span: Span,
                                        trait_ref: ty::TraitRef<'tcx>,
                                        bounds: &[ast::TyParamBound])
                                        -> Ty<'tcx>
    where AC : AstConv<'tcx>, RS : RegionScope
{
    let existential_bounds = conv_existential_bounds(this,
                                                     rscope,
                                                     span,
                                                     &[Rc::new(trait_ref.clone())],
                                                     bounds);

    let result = ty::mk_trait(this.tcx(), trait_ref, existential_bounds);
    debug!("trait_ref_to_object_type: result={}",
           result.repr(this.tcx()));

    result
}

fn qpath_to_ty<'tcx,AC,RS>(this: &AC,
                           rscope: &RS,
                           ast_ty: &ast::Ty, // the TyQPath
                           qpath: &ast::QPath)
                           -> Ty<'tcx>
    where AC: AstConv<'tcx>, RS: RegionScope
{
    debug!("qpath_to_ty(ast_ty={})",
           ast_ty.repr(this.tcx()));

    let self_type = ast_ty_to_ty(this, rscope, &*qpath.self_type);

    debug!("qpath_to_ty: self_type={}", self_type.repr(this.tcx()));

    let trait_ref = instantiate_trait_ref(this,
                                          rscope,
                                          &*qpath.trait_ref,
                                          Some(self_type));

    debug!("qpath_to_ty: trait_ref={}", trait_ref.repr(this.tcx()));

    let trait_def = this.get_trait_def(trait_ref.def_id);

    for ty_param_def in trait_def.generics.types.get_slice(AssocSpace).iter() {
        if ty_param_def.name == qpath.item_name.name {
            debug!("qpath_to_ty: corresponding ty_param_def={}", ty_param_def);
            return trait_ref.substs.type_for_def(ty_param_def);
        }
    }

    this.tcx().sess.span_bug(ast_ty.span,
                             "this associated type didn't get added \
                              as a parameter for some reason")
}

// Parses the programmer's textual representation of a type into our
// internal notion of a type.
pub fn ast_ty_to_ty<'tcx, AC: AstConv<'tcx>, RS: RegionScope>(
        this: &AC, rscope: &RS, ast_ty: &ast::Ty) -> Ty<'tcx>
{
    debug!("ast_ty_to_ty(ast_ty={})",
           ast_ty.repr(this.tcx()));

    let tcx = this.tcx();

    let mut ast_ty_to_ty_cache = tcx.ast_ty_to_ty_cache.borrow_mut();
    match ast_ty_to_ty_cache.get(&ast_ty.id) {
        Some(&ty::atttce_resolved(ty)) => return ty,
        Some(&ty::atttce_unresolved) => {
            tcx.sess.span_fatal(ast_ty.span,
                                "illegal recursive type; insert an enum \
                                 or struct in the cycle, if this is \
                                 desired");
        }
        None => { /* go on */ }
    }
    ast_ty_to_ty_cache.insert(ast_ty.id, ty::atttce_unresolved);
    drop(ast_ty_to_ty_cache);

    let typ = ast_ty_to_builtin_ty(this, rscope, ast_ty).unwrap_or_else(|| {
        match ast_ty.node {
            ast::TyVec(ref ty) => {
                ty::mk_vec(tcx, ast_ty_to_ty(this, rscope, &**ty), None)
            }
            ast::TyObjectSum(ref ty, ref bounds) => {
                match ast_ty_to_trait_ref(this, rscope, &**ty, bounds.as_slice()) {
                    Ok(trait_ref) => {
                        trait_ref_to_object_type(this, rscope, ast_ty.span,
                                                 trait_ref, bounds.as_slice())
                    }
                    Err(ErrorReported) => {
                        ty::mk_err()
                    }
                }
            }
            ast::TyPtr(ref mt) => {
                ty::mk_ptr(tcx, ty::mt {
                    ty: ast_ty_to_ty(this, rscope, &*mt.ty),
                    mutbl: mt.mutbl
                })
            }
            ast::TyRptr(ref region, ref mt) => {
                let r = opt_ast_region_to_region(this, rscope, ast_ty.span, region);
                debug!("ty_rptr r={}", r.repr(this.tcx()));
                let t = ast_ty_to_ty(this, rscope, &*mt.ty);
                ty::mk_rptr(tcx, r, ty::mt {ty: t, mutbl: mt.mutbl})
            }
            ast::TyTup(ref fields) => {
                let flds = fields.iter()
                                 .map(|t| ast_ty_to_ty(this, rscope, &**t))
                                 .collect();
                ty::mk_tup(tcx, flds)
            }
            ast::TyParen(ref typ) => ast_ty_to_ty(this, rscope, &**typ),
            ast::TyBareFn(ref bf) => {
                if bf.decl.variadic && bf.abi != abi::C {
                    tcx.sess.span_err(ast_ty.span,
                                      "variadic function must have C calling convention");
                }
                ty::mk_bare_fn(tcx, ty_of_bare_fn(this, bf.fn_style, bf.abi, &*bf.decl))
            }
            ast::TyClosure(ref f) => {
                // Use corresponding trait store to figure out default bounds
                // if none were specified.
                let bounds = conv_existential_bounds(this,
                                                     rscope,
                                                     ast_ty.span,
                                                     [].as_slice(),
                                                     f.bounds.as_slice());
                let fn_decl = ty_of_closure(this,
                                            f.fn_style,
                                            f.onceness,
                                            bounds,
                                            ty::RegionTraitStore(
                                                bounds.region_bound,
                                                ast::MutMutable),
                                            &*f.decl,
                                            abi::Rust,
                                            None);
                ty::mk_closure(tcx, fn_decl)
            }
            ast::TyProc(ref f) => {
                // Use corresponding trait store to figure out default bounds
                // if none were specified.
                let bounds = conv_existential_bounds(this, rscope,
                                                     ast_ty.span,
                                                     [].as_slice(),
                                                     f.bounds.as_slice());

                let fn_decl = ty_of_closure(this,
                                            f.fn_style,
                                            f.onceness,
                                            bounds,
                                            ty::UniqTraitStore,
                                            &*f.decl,
                                            abi::Rust,
                                            None);

                ty::mk_closure(tcx, fn_decl)
            }
            ast::TyPolyTraitRef(ref bounds) => {
                conv_ty_poly_trait_ref(this, rscope, ast_ty.span, bounds.as_slice())
            }
            ast::TyPath(ref path, id) => {
                let a_def = match tcx.def_map.borrow().get(&id) {
                    None => {
                        tcx.sess
                           .span_bug(ast_ty.span,
                                     format!("unbound path {}",
                                             path.repr(tcx)).as_slice())
                    }
                    Some(&d) => d
                };
                match a_def {
                    def::DefTrait(trait_def_id) => {
                        // N.B. this case overlaps somewhat with
                        // TyObjectSum, see that fn for details
                        let result = ast_path_to_trait_ref(this,
                                                           rscope,
                                                           trait_def_id,
                                                           None,
                                                           path);
                        trait_ref_to_object_type(this, rscope, path.span, result, &[])
                    }
                    def::DefTy(did, _) | def::DefStruct(did) => {
                        ast_path_to_ty(this, rscope, did, path).ty
                    }
                    def::DefTyParam(space, id, n) => {
                        check_path_args(tcx, path, NO_TPS | NO_REGIONS);
                        ty::mk_param(tcx, space, n, id)
                    }
                    def::DefSelfTy(id) => {
                        // n.b.: resolve guarantees that the this type only appears in a
                        // trait, which we rely upon in various places when creating
                        // substs
                        check_path_args(tcx, path, NO_TPS | NO_REGIONS);
                        let did = ast_util::local_def(id);
                        ty::mk_self_type(tcx, did)
                    }
                    def::DefMod(id) => {
                        tcx.sess.span_fatal(ast_ty.span,
                            format!("found module name used as a type: {}",
                                    tcx.map.node_to_string(id.node)).as_slice());
                    }
                    def::DefPrimTy(_) => {
                        panic!("DefPrimTy arm missed in previous ast_ty_to_prim_ty call");
                    }
                    def::DefAssociatedTy(trait_type_id) => {
                        let path_str = tcx.map.path_to_string(
                            tcx.map.get_parent(trait_type_id.node));
                        tcx.sess.span_err(ast_ty.span,
                                          format!("ambiguous associated \
                                                   type; specify the type \
                                                   using the syntax `<Type \
                                                   as {}>::{}`",
                                                  path_str,
                                                  token::get_ident(
                                                      path.segments
                                                          .last()
                                                          .unwrap()
                                                          .identifier)
                                                  .get()).as_slice());
                        ty::mk_err()
                    }
                    _ => {
                        tcx.sess.span_fatal(ast_ty.span,
                                            format!("found value name used \
                                                     as a type: {}",
                                                    a_def).as_slice());
                    }
                }
            }
            ast::TyQPath(ref qpath) => {
                qpath_to_ty(this, rscope, ast_ty, &**qpath)
            }
            ast::TyFixedLengthVec(ref ty, ref e) => {
                match const_eval::eval_const_expr_partial(tcx, &**e) {
                    Ok(ref r) => {
                        match *r {
                            const_eval::const_int(i) =>
                                ty::mk_vec(tcx, ast_ty_to_ty(this, rscope, &**ty),
                                           Some(i as uint)),
                            const_eval::const_uint(i) =>
                                ty::mk_vec(tcx, ast_ty_to_ty(this, rscope, &**ty),
                                           Some(i as uint)),
                            _ => {
                                tcx.sess.span_fatal(
                                    ast_ty.span, "expected constant expr for array length");
                            }
                        }
                    }
                    Err(ref r) => {
                        tcx.sess.span_fatal(
                            ast_ty.span,
                            format!("expected constant expr for array \
                                     length: {}",
                                    *r).as_slice());
                    }
                }
            }
            ast::TyTypeof(ref _e) => {
                tcx.sess.span_bug(ast_ty.span, "typeof is reserved but unimplemented");
            }
            ast::TyInfer => {
                // TyInfer also appears as the type of arguments or return
                // values in a ExprClosure or ExprProc, or as
                // the type of local variables. Both of these cases are
                // handled specially and will not descend into this routine.
                this.ty_infer(ast_ty.span)
            }
        }
    });

    tcx.ast_ty_to_ty_cache.borrow_mut().insert(ast_ty.id, ty::atttce_resolved(typ));
    return typ;
}

pub fn ty_of_arg<'tcx, AC: AstConv<'tcx>, RS: RegionScope>(this: &AC, rscope: &RS,
                                                           a: &ast::Arg,
                                                           expected_ty: Option<Ty<'tcx>>)
                                                           -> Ty<'tcx> {
    match a.ty.node {
        ast::TyInfer if expected_ty.is_some() => expected_ty.unwrap(),
        ast::TyInfer => this.ty_infer(a.ty.span),
        _ => ast_ty_to_ty(this, rscope, &*a.ty),
    }
}

struct SelfInfo<'a, 'tcx> {
    untransformed_self_ty: Ty<'tcx>,
    explicit_self: &'a ast::ExplicitSelf,
}

pub fn ty_of_method<'tcx, AC: AstConv<'tcx>>(
                    this: &AC,
                    fn_style: ast::FnStyle,
                    untransformed_self_ty: Ty<'tcx>,
                    explicit_self: &ast::ExplicitSelf,
                    decl: &ast::FnDecl,
                    abi: abi::Abi)
                    -> (ty::BareFnTy<'tcx>, ty::ExplicitSelfCategory) {
    let self_info = Some(SelfInfo {
        untransformed_self_ty: untransformed_self_ty,
        explicit_self: explicit_self,
    });
    let (bare_fn_ty, optional_explicit_self_category) =
        ty_of_method_or_bare_fn(this,
                                fn_style,
                                abi,
                                self_info,
                                decl);
    (bare_fn_ty, optional_explicit_self_category.unwrap())
}

pub fn ty_of_bare_fn<'tcx, AC: AstConv<'tcx>>(this: &AC, fn_style: ast::FnStyle, abi: abi::Abi,
                                              decl: &ast::FnDecl) -> ty::BareFnTy<'tcx> {
    let (bare_fn_ty, _) = ty_of_method_or_bare_fn(this, fn_style, abi, None, decl);
    bare_fn_ty
}

fn ty_of_method_or_bare_fn<'a, 'tcx, AC: AstConv<'tcx>>(
                           this: &AC,
                           fn_style: ast::FnStyle,
                           abi: abi::Abi,
                           opt_self_info: Option<SelfInfo<'a, 'tcx>>,
                           decl: &ast::FnDecl)
                           -> (ty::BareFnTy<'tcx>,
                               Option<ty::ExplicitSelfCategory>) {
    debug!("ty_of_method_or_bare_fn");

    // New region names that appear inside of the arguments of the function
    // declaration are bound to that function type.
    let rb = rscope::BindingRscope::new();

    // `implied_output_region` is the region that will be assumed for any
    // region parameters in the return type. In accordance with the rules for
    // lifetime elision, we can determine it in two ways. First (determined
    // here), if self is by-reference, then the implied output region is the
    // region of the self parameter.
    let mut explicit_self_category_result = None;
    let (self_ty, mut implied_output_region) = match opt_self_info {
        None => (None, None),
        Some(self_info) => {
            // Figure out and record the explicit self category.
            let explicit_self_category =
                determine_explicit_self_category(this, &rb, &self_info);
            explicit_self_category_result = Some(explicit_self_category);
            match explicit_self_category {
                ty::StaticExplicitSelfCategory => {
                    (None, None)
                }
                ty::ByValueExplicitSelfCategory => {
                    (Some(self_info.untransformed_self_ty), None)
                }
                ty::ByReferenceExplicitSelfCategory(region, mutability) => {
                    (Some(ty::mk_rptr(this.tcx(),
                                      region,
                                      ty::mt {
                                        ty: self_info.untransformed_self_ty,
                                        mutbl: mutability
                                      })),
                     Some(region))
                }
                ty::ByBoxExplicitSelfCategory => {
                    (Some(ty::mk_uniq(this.tcx(),
                                      self_info.untransformed_self_ty)),
                     None)
                }
            }
        }
    };

    // HACK(eddyb) replace the fake self type in the AST with the actual type.
    let input_params = if self_ty.is_some() {
        decl.inputs.slice_from(1)
    } else {
        decl.inputs.as_slice()
    };
    let input_tys = input_params.iter().map(|a| ty_of_arg(this, &rb, a, None));
    let input_pats: Vec<String> = input_params.iter()
                                              .map(|a| pprust::pat_to_string(&*a.pat))
                                              .collect();
    let self_and_input_tys: Vec<Ty> =
        self_ty.into_iter().chain(input_tys).collect();

    let mut lifetimes_for_params: Vec<(String, Vec<ty::Region>)> = Vec::new();

    // Second, if there was exactly one lifetime (either a substitution or a
    // reference) in the arguments, then any anonymous regions in the output
    // have that lifetime.
    if implied_output_region.is_none() {
        let mut self_and_input_tys_iter = self_and_input_tys.iter();
        if self_ty.is_some() {
            // Skip the first argument if `self` is present.
            drop(self_and_input_tys_iter.next())
        }

        for (input_type, input_pat) in self_and_input_tys_iter.zip(input_pats.into_iter()) {
            let mut accumulator = Vec::new();
            ty::accumulate_lifetimes_in_type(&mut accumulator, *input_type);
            lifetimes_for_params.push((input_pat, accumulator));
        }

        if lifetimes_for_params.iter().map(|&(_, ref x)| x.len()).sum() == 1 {
            implied_output_region =
                Some(lifetimes_for_params.iter()
                                         .filter_map(|&(_, ref x)|
                                            if x.len() == 1 { Some(x[0]) } else { None })
                                         .next().unwrap());
        }
    }

    let param_lifetimes: Vec<(String, uint)> = lifetimes_for_params.into_iter()
                                                                   .map(|(n, v)| (n, v.len()))
                                                                   .filter(|&(_, l)| l != 0)
                                                                   .collect();

    let output_ty = match decl.output {
        ast::Return(ref output) if output.node == ast::TyInfer =>
            ty::FnConverging(this.ty_infer(output.span)),
        ast::Return(ref output) =>
            ty::FnConverging(match implied_output_region {
                Some(implied_output_region) => {
                    let rb = SpecificRscope::new(implied_output_region);
                    ast_ty_to_ty(this, &rb, &**output)
                }
                None => {
                    // All regions must be explicitly specified in the output
                    // if the lifetime elision rules do not apply. This saves
                    // the user from potentially-confusing errors.
                    let rb = UnelidableRscope::new(param_lifetimes);
                    ast_ty_to_ty(this, &rb, &**output)
                }
            }),
        ast::NoReturn(_) => ty::FnDiverging
    };

    (ty::BareFnTy {
        fn_style: fn_style,
        abi: abi,
        sig: ty::FnSig {
            inputs: self_and_input_tys,
            output: output_ty,
            variadic: decl.variadic
        }
    }, explicit_self_category_result)
}

fn determine_explicit_self_category<'a, 'tcx, AC: AstConv<'tcx>,
                                    RS:RegionScope>(
                                    this: &AC,
                                    rscope: &RS,
                                    self_info: &SelfInfo<'a, 'tcx>)
                                    -> ty::ExplicitSelfCategory
{
    return match self_info.explicit_self.node {
        ast::SelfStatic => ty::StaticExplicitSelfCategory,
        ast::SelfValue(_) => ty::ByValueExplicitSelfCategory,
        ast::SelfRegion(ref lifetime, mutability, _) => {
            let region =
                opt_ast_region_to_region(this,
                                         rscope,
                                         self_info.explicit_self.span,
                                         lifetime);
            ty::ByReferenceExplicitSelfCategory(region, mutability)
        }
        ast::SelfExplicit(ref ast_type, _) => {
            let explicit_type = ast_ty_to_ty(this, rscope, &**ast_type);

            // We wish to (for now) categorize an explicit self
            // declaration like `self: SomeType` into either `self`,
            // `&self`, `&mut self`, or `Box<self>`. We do this here
            // by some simple pattern matching. A more precise check
            // is done later in `check_method_self_type()`.
            //
            // Examples:
            //
            // ```
            // impl Foo for &T {
            //     // Legal declarations:
            //     fn method1(self: &&T); // ByReferenceExplicitSelfCategory
            //     fn method2(self: &T); // ByValueExplicitSelfCategory
            //     fn method3(self: Box<&T>); // ByBoxExplicitSelfCategory
            //
            //     // Invalid cases will be caught later by `check_method_self_type`:
            //     fn method_err1(self: &mut T); // ByReferenceExplicitSelfCategory
            // }
            // ```
            //
            // To do the check we just count the number of "modifiers"
            // on each type and compare them. If they are the same or
            // the impl has more, we call it "by value". Otherwise, we
            // look at the outermost modifier on the method decl and
            // call it by-ref, by-box as appropriate. For method1, for
            // example, the impl type has one modifier, but the method
            // type has two, so we end up with
            // ByReferenceExplicitSelfCategory.

            let impl_modifiers = count_modifiers(self_info.untransformed_self_ty);
            let method_modifiers = count_modifiers(explicit_type);

            debug!("determine_explicit_self_category(self_info.untransformed_self_ty={} \
                   explicit_type={} \
                   modifiers=({},{})",
                   self_info.untransformed_self_ty.repr(this.tcx()),
                   explicit_type.repr(this.tcx()),
                   impl_modifiers,
                   method_modifiers);

            if impl_modifiers >= method_modifiers {
                ty::ByValueExplicitSelfCategory
            } else {
                match explicit_type.sty {
                    ty::ty_rptr(r, mt) => ty::ByReferenceExplicitSelfCategory(r, mt.mutbl),
                    ty::ty_uniq(_) => ty::ByBoxExplicitSelfCategory,
                    _ => ty::ByValueExplicitSelfCategory,
                }
            }
        }
    };

    fn count_modifiers(ty: Ty) -> uint {
        match ty.sty {
            ty::ty_rptr(_, mt) => count_modifiers(mt.ty) + 1,
            ty::ty_uniq(t) => count_modifiers(t) + 1,
            _ => 0,
        }
    }
}

pub fn ty_of_closure<'tcx, AC: AstConv<'tcx>>(
    this: &AC,
    fn_style: ast::FnStyle,
    onceness: ast::Onceness,
    bounds: ty::ExistentialBounds,
    store: ty::TraitStore,
    decl: &ast::FnDecl,
    abi: abi::Abi,
    expected_sig: Option<ty::FnSig<'tcx>>)
    -> ty::ClosureTy<'tcx>
{
    debug!("ty_of_closure(expected_sig={})",
           expected_sig.repr(this.tcx()));

    // new region names that appear inside of the fn decl are bound to
    // that function type
    let rb = rscope::BindingRscope::new();

    let input_tys: Vec<_> = decl.inputs.iter().enumerate().map(|(i, a)| {
        let expected_arg_ty = expected_sig.as_ref().and_then(|e| {
            // no guarantee that the correct number of expected args
            // were supplied
            if i < e.inputs.len() {
                Some(e.inputs[i])
            } else {
                None
            }
        });
        ty_of_arg(this, &rb, a, expected_arg_ty)
    }).collect();

    let expected_ret_ty = expected_sig.map(|e| e.output);

    let output_ty = match decl.output {
        ast::Return(ref output) if output.node == ast::TyInfer && expected_ret_ty.is_some() =>
            expected_ret_ty.unwrap(),
        ast::Return(ref output) if output.node == ast::TyInfer =>
            ty::FnConverging(this.ty_infer(output.span)),
        ast::Return(ref output) =>
            ty::FnConverging(ast_ty_to_ty(this, &rb, &**output)),
        ast::NoReturn(_) => ty::FnDiverging
    };

    debug!("ty_of_closure: input_tys={}", input_tys.repr(this.tcx()));
    debug!("ty_of_closure: output_ty={}", output_ty.repr(this.tcx()));

    ty::ClosureTy {
        fn_style: fn_style,
        onceness: onceness,
        store: store,
        bounds: bounds,
        abi: abi,
        sig: ty::FnSig {inputs: input_tys,
                        output: output_ty,
                        variadic: decl.variadic}
    }
}

/// Given an existential type like `Foo+'a+Bar`, this routine converts the `'a` and `Bar` intos an
/// `ExistentialBounds` struct. The `main_trait_refs` argument specifies the `Foo` -- it is absent
/// for closures. Eventually this should all be normalized, I think, so that there is no "main
/// trait ref" and instead we just have a flat list of bounds as the existential type.
pub fn conv_existential_bounds<'tcx, AC: AstConv<'tcx>, RS:RegionScope>(
    this: &AC,
    rscope: &RS,
    span: Span,
    main_trait_refs: &[Rc<ty::TraitRef<'tcx>>],
    ast_bounds: &[ast::TyParamBound])
    -> ty::ExistentialBounds
{
    let ast_bound_refs: Vec<&ast::TyParamBound> =
        ast_bounds.iter().collect();

    let partitioned_bounds =
        partition_bounds(this.tcx(), span, ast_bound_refs.as_slice());

    conv_existential_bounds_from_partitioned_bounds(
        this, rscope, span, main_trait_refs, partitioned_bounds)
}

fn conv_ty_poly_trait_ref<'tcx, AC, RS>(
    this: &AC,
    rscope: &RS,
    span: Span,
    ast_bounds: &[ast::TyParamBound])
    -> Ty<'tcx>
    where AC: AstConv<'tcx>, RS:RegionScope
{
    let ast_bounds: Vec<&ast::TyParamBound> = ast_bounds.iter().collect();
    let mut partitioned_bounds = partition_bounds(this.tcx(), span, ast_bounds[]);

    let main_trait_bound = match partitioned_bounds.trait_bounds.remove(0) {
        Some(trait_bound) => {
            Some(instantiate_poly_trait_ref(this, rscope, trait_bound, None))
        }
        None => {
            this.tcx().sess.span_err(
                span,
                "at least one non-builtin trait is required for an object type");
            None
        }
    };

    let bounds = conv_existential_bounds_from_partitioned_bounds(this,
                                                                 rscope,
                                                                 span,
                                                                 main_trait_bound.as_slice(),
                                                                 partitioned_bounds);

    match main_trait_bound {
        None => ty::mk_err(),
        Some(principal) => ty::mk_trait(this.tcx(), (*principal).clone(), bounds)
    }
}

pub fn conv_existential_bounds_from_partitioned_bounds<'tcx, AC, RS>(
    this: &AC,
    rscope: &RS,
    span: Span,
    main_trait_refs: &[Rc<ty::TraitRef<'tcx>>],
    partitioned_bounds: PartitionedBounds)
    -> ty::ExistentialBounds
    where AC: AstConv<'tcx>, RS:RegionScope
{
    let PartitionedBounds { builtin_bounds,
                            trait_bounds,
                            region_bounds } =
        partitioned_bounds;

    if !trait_bounds.is_empty() {
        let b = &trait_bounds[0];
        this.tcx().sess.span_err(
            b.trait_ref.path.span,
            format!("only the builtin traits can be used \
                     as closure or object bounds").as_slice());
    }

    // The "main trait refs", rather annoyingly, have no type
    // specified for the `Self` parameter of the trait. The reason for
    // this is that they are, after all, *existential* types, and
    // hence that type is unknown. However, leaving this type missing
    // causes the substitution code to go all awry when walking the
    // bounds, so here we clone those trait refs and insert ty::err as
    // the self type. Perhaps we should do this more generally, it'd
    // be convenient (or perhaps something else, i.e., ty::erased).
    let main_trait_refs: Vec<Rc<ty::TraitRef>> =
        main_trait_refs.iter()
        .map(|t|
             Rc::new(ty::TraitRef {
                 def_id: t.def_id,
                 substs: t.substs.with_self_ty(ty::mk_err()) }))
        .collect();

    let region_bound = compute_region_bound(this,
                                            rscope,
                                            span,
                                            builtin_bounds,
                                            region_bounds.as_slice(),
                                            main_trait_refs.as_slice());

    ty::ExistentialBounds {
        region_bound: region_bound,
        builtin_bounds: builtin_bounds,
    }
}

/// Given the bounds on a type parameter / existential type, determines what single region bound
/// (if any) we can use to summarize this type. The basic idea is that we will use the bound the
/// user provided, if they provided one, and otherwise search the supertypes of trait bounds for
/// region bounds. It may be that we can derive no bound at all, in which case we return `None`.
pub fn compute_opt_region_bound<'tcx>(tcx: &ty::ctxt<'tcx>,
                                      span: Span,
                                      builtin_bounds: ty::BuiltinBounds,
                                      region_bounds: &[&ast::Lifetime],
                                      trait_bounds: &[Rc<ty::TraitRef<'tcx>>])
                                      -> Option<ty::Region>
{
    if region_bounds.len() > 1 {
        tcx.sess.span_err(
            region_bounds[1].span,
            format!("only a single explicit lifetime bound is permitted").as_slice());
    }

    if region_bounds.len() != 0 {
        // Explicitly specified region bound. Use that.
        let r = region_bounds[0];
        return Some(ast_region_to_region(tcx, r));
    }

    // No explicit region bound specified. Therefore, examine trait
    // bounds and see if we can derive region bounds from those.
    let derived_region_bounds =
        ty::required_region_bounds(
            tcx,
            &[],
            builtin_bounds,
            trait_bounds);

    // If there are no derived region bounds, then report back that we
    // can find no region bound.
    if derived_region_bounds.len() == 0 {
        return None;
    }

    // If any of the derived region bounds are 'static, that is always
    // the best choice.
    if derived_region_bounds.iter().any(|r| ty::ReStatic == *r) {
        return Some(ty::ReStatic);
    }

    // Determine whether there is exactly one unique region in the set
    // of derived region bounds. If so, use that. Otherwise, report an
    // error.
    let r = derived_region_bounds[0];
    if derived_region_bounds.slice_from(1).iter().any(|r1| r != *r1) {
        tcx.sess.span_err(
            span,
            format!("ambiguous lifetime bound, \
                     explicit lifetime bound required").as_slice());
    }
    return Some(r);
}

/// A version of `compute_opt_region_bound` for use where some region bound is required
/// (existential types, basically). Reports an error if no region bound can be derived and we are
/// in an `rscope` that does not provide a default.
fn compute_region_bound<'tcx, AC: AstConv<'tcx>, RS:RegionScope>(
    this: &AC,
    rscope: &RS,
    span: Span,
    builtin_bounds: ty::BuiltinBounds,
    region_bounds: &[&ast::Lifetime],
    trait_bounds: &[Rc<ty::TraitRef<'tcx>>])
    -> ty::Region
{
    match compute_opt_region_bound(this.tcx(), span, builtin_bounds,
                                   region_bounds, trait_bounds) {
        Some(r) => r,
        None => {
            match rscope.default_region_bound(span) {
                Some(r) => { r }
                None => {
                    this.tcx().sess.span_err(
                        span,
                        format!("explicit lifetime bound required").as_slice());
                    ty::ReStatic
                }
            }
        }
    }
}

pub struct PartitionedBounds<'a> {
    pub builtin_bounds: ty::BuiltinBounds,
    pub trait_bounds: Vec<&'a ast::PolyTraitRef>,
    pub region_bounds: Vec<&'a ast::Lifetime>,
}

/// Divides a list of bounds from the AST into three groups: builtin bounds (Copy, Sized etc),
/// general trait bounds, and region bounds.
pub fn partition_bounds<'a>(tcx: &ty::ctxt,
                            _span: Span,
                            ast_bounds: &'a [&ast::TyParamBound])
                            -> PartitionedBounds<'a>
{
    let mut builtin_bounds = ty::empty_builtin_bounds();
    let mut region_bounds = Vec::new();
    let mut trait_bounds = Vec::new();
    let mut trait_def_ids = DefIdMap::new();
    for &ast_bound in ast_bounds.iter() {
        match *ast_bound {
            ast::TraitTyParamBound(ref b) => {
                match ::lookup_def_tcx(tcx, b.trait_ref.path.span, b.trait_ref.ref_id) {
                    def::DefTrait(trait_did) => {
                        match trait_def_ids.get(&trait_did) {
                            // Already seen this trait. We forbid
                            // duplicates in the list (for some
                            // reason).
                            Some(span) => {
                                span_err!(
                                    tcx.sess, b.trait_ref.path.span, E0127,
                                    "trait `{}` already appears in the \
                                     list of bounds",
                                    b.trait_ref.path.user_string(tcx));
                                tcx.sess.span_note(
                                    *span,
                                    "previous appearance is here");

                                continue;
                            }

                            None => { }
                        }

                        trait_def_ids.insert(trait_did, b.trait_ref.path.span);

                        if ty::try_add_builtin_trait(tcx,
                                                     trait_did,
                                                     &mut builtin_bounds) {
                            continue; // success
                        }
                    }
                    _ => {
                        // Not a trait? that's an error, but it'll get
                        // reported later.
                    }
                }
                trait_bounds.push(b);
            }
            ast::RegionTyParamBound(ref l) => {
                region_bounds.push(l);
            }
        }
    }

    PartitionedBounds {
        builtin_bounds: builtin_bounds,
        trait_bounds: trait_bounds,
        region_bounds: region_bounds,
    }
}
