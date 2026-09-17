//! `#[derive(Gob)]` for [gobwire](https://docs.rs/gobwire).
//!
//! Generates `GobType`, `Encode` and `Decode` for a struct with named fields, mapping each field
//! to the Go field of the same name.
//!
//! Attributes:
//!
//! * `#[gob(name = "Post")]` on the struct: the type name sent in its definition (informational;
//!   Go's decoder does not match on it). Defaults to the Rust name.
//! * `#[gob(name = "Id")]` on a field: the Go field name, which **is** matched. Defaults to the
//!   field name converted from snake_case to PascalCase (`request_id` → `RequestId`), which is
//!   wrong for Go initialisms such as `IPAddress` — name those explicitly.
//! * `#[gob(skip)]` on a field: neither sent nor received, like an unexported Go field.
//! * `#[gob(transparent)]` on a struct with exactly one field: encode as that field, for Go
//!   named types such as `type StringMap map[string]string`.

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote;
use syn::{Data, DeriveInput, Fields, LitStr, parse_macro_input, spanned::Spanned};

#[proc_macro_derive(Gob, attributes(gob))]
pub fn derive_gob(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(&input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

#[derive(Default)]
struct ContainerAttrs {
    name: Option<String>,
    transparent: bool,
}

#[derive(Default)]
struct FieldAttrs {
    name: Option<String>,
    skip: bool,
}

fn container_attrs(input: &DeriveInput) -> syn::Result<ContainerAttrs> {
    let mut out = ContainerAttrs::default();
    for attr in input.attrs.iter().filter(|a| a.path().is_ident("gob")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                out.name = Some(meta.value()?.parse::<LitStr>()?.value());
                Ok(())
            } else if meta.path.is_ident("transparent") {
                out.transparent = true;
                Ok(())
            } else {
                Err(meta.error("unknown gob container attribute; expected `name` or `transparent`"))
            }
        })?;
    }
    Ok(out)
}

fn field_attrs(field: &syn::Field) -> syn::Result<FieldAttrs> {
    let mut out = FieldAttrs::default();
    for attr in field.attrs.iter().filter(|a| a.path().is_ident("gob")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                out.name = Some(meta.value()?.parse::<LitStr>()?.value());
                Ok(())
            } else if meta.path.is_ident("skip") {
                out.skip = true;
                Ok(())
            } else {
                Err(meta.error("unknown gob field attribute; expected `name` or `skip`"))
            }
        })?;
    }
    Ok(out)
}

fn pascal_case(ident: &str) -> String {
    let ident = ident.strip_prefix("r#").unwrap_or(ident);
    let mut out = String::with_capacity(ident.len());
    let mut upper = true;
    for c in ident.chars() {
        if c == '_' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

fn expand(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let attrs = container_attrs(input)?;
    let ident = &input.ident;
    let go_name = attrs.name.clone().unwrap_or_else(|| ident.to_string());
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new(
            input.span(),
            "#[derive(Gob)] supports structs only",
        ));
    };

    if attrs.transparent {
        return expand_transparent(input, &data.fields);
    }

    let named: Vec<&syn::Field> = match &data.fields {
        Fields::Named(f) => f.named.iter().collect(),
        Fields::Unit => Vec::new(),
        Fields::Unnamed(_) => {
            return Err(syn::Error::new(
                input.span(),
                "#[derive(Gob)] needs named fields; use #[gob(transparent)] for a one-field tuple struct",
            ));
        }
    };

    let mut members = Vec::new();
    let mut names = Vec::new();
    let mut types = Vec::new();
    for field in named {
        let fa = field_attrs(field)?;
        if fa.skip {
            continue;
        }
        let Some(member) = field.ident.clone() else {
            return Err(syn::Error::new(field.span(), "expected a named field"));
        };
        let name = fa.name.unwrap_or_else(|| pascal_case(&member.to_string()));
        if name.is_empty() {
            return Err(syn::Error::new(
                field.span(),
                "a gob field name cannot be empty",
            ));
        }
        if names.contains(&name) {
            return Err(syn::Error::new(
                field.span(),
                format!("duplicate gob field name {name:?}"),
            ));
        }
        members.push(member);
        names.push(name);
        types.push(field.ty.clone());
    }

    let indices: Vec<u32> = (0..members.len() as u32).collect();
    let local_indices: Vec<usize> = (0..members.len()).collect();
    let name_lits: Vec<LitStr> = names
        .iter()
        .map(|n| LitStr::new(n, Span::call_site()))
        .collect();
    let go_name_lit = LitStr::new(&go_name, Span::call_site());

    // Bound the type parameters, not the field types: a bound on a field type is circular for a
    // recursive struct (`kids: Vec<Option<Self>>`) and the trait solver overflows on it.
    let (gobtype_where, encode_where, decode_where) = param_bounds(input, where_clause);

    Ok(quote! {
        #[automatically_derived]
        impl #impl_generics ::gobwire::GobType for #ident #ty_generics #gobtype_where {
            fn describe(d: &mut ::gobwire::Describer<'_>) -> ::gobwire::Result<i64> {
                d.structure(::core::any::type_name::<Self>(), #go_name_lit, |d| {
                    ::core::result::Result::Ok(::std::vec![
                        #( (#name_lits, <#types as ::gobwire::GobType>::describe(d)?), )*
                    ])
                })
            }

            fn compatible(types: &::gobwire::TypeTable, wire: i64) -> bool {
                types.is_struct(wire)
            }
        }

        #[automatically_derived]
        impl #impl_generics ::gobwire::Encode for #ident #ty_generics #encode_where {
            fn describe_value(&self, d: &mut ::gobwire::Describer<'_>) -> ::gobwire::Result<i64> {
                <Self as ::gobwire::GobType>::describe(d)
            }

            /// Go never omits a struct-kind field, even a zero one.
            fn is_zero(&self) -> bool {
                false
            }

            fn frames_as_struct(&self) -> bool {
                true
            }

            fn encode(&self, e: &mut ::gobwire::ValueEncoder<'_>) -> ::gobwire::Result<()> {
                let mut s = e.structure();
                #( s.field(#indices, &self.#members)?; )*
                s.end();
                ::core::result::Result::Ok(())
            }
        }

        #[automatically_derived]
        impl #impl_generics ::gobwire::Decode for #ident #ty_generics #decode_where {
            fn decode_into(&mut self, d: &mut ::gobwire::ValueDecoder<'_>, wire: i64) -> ::gobwire::Result<()> {
                const NAMES: &[&str] = &[#(#name_lits),*];
                let plan = d.struct_plan(::core::any::type_name::<Self>(), wire, NAMES, |types, i, w| {
                    match i {
                        #( #local_indices => <#types as ::gobwire::GobType>::compatible(types, w), )*
                        _ => false,
                    }
                })?;
                let mut s = d.structure(plan)?;
                while let ::core::option::Option::Some(field) = s.next_field()? {
                    match field.local {
                        #( ::core::option::Option::Some(#local_indices) => {
                            ::gobwire::Decode::decode_into(&mut self.#members, s.decoder(), field.wire)?
                        } )*
                        _ => s.decoder().skip(field.wire)?,
                    }
                }
                ::core::result::Result::Ok(())
            }
        }
    })
}

fn expand_transparent(input: &DeriveInput, fields: &Fields) -> syn::Result<TokenStream2> {
    let ident = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    let (member, ty): (TokenStream2, syn::Type) = match fields {
        Fields::Named(f) if f.named.len() == 1 => {
            let field = &f.named[0];
            let name = field.ident.clone();
            (quote!(#name), field.ty.clone())
        }
        Fields::Unnamed(f) if f.unnamed.len() == 1 => (quote!(0), f.unnamed[0].ty.clone()),
        _ => {
            return Err(syn::Error::new(
                input.span(),
                "#[gob(transparent)] needs exactly one field",
            ));
        }
    };
    let (gobtype_where, encode_where, decode_where) = param_bounds(input, where_clause);

    Ok(quote! {
        #[automatically_derived]
        impl #impl_generics ::gobwire::GobType for #ident #ty_generics #gobtype_where {
            fn describe(d: &mut ::gobwire::Describer<'_>) -> ::gobwire::Result<i64> {
                <#ty as ::gobwire::GobType>::describe(d)
            }
            fn compatible(types: &::gobwire::TypeTable, wire: i64) -> bool {
                <#ty as ::gobwire::GobType>::compatible(types, wire)
            }
        }

        #[automatically_derived]
        impl #impl_generics ::gobwire::Encode for #ident #ty_generics #encode_where {
            fn describe_value(&self, d: &mut ::gobwire::Describer<'_>) -> ::gobwire::Result<i64> {
                ::gobwire::Encode::describe_value(&self.#member, d)
            }
            fn is_zero(&self) -> bool {
                ::gobwire::Encode::is_zero(&self.#member)
            }
            fn frames_as_struct(&self) -> bool {
                ::gobwire::Encode::frames_as_struct(&self.#member)
            }
            fn encode(&self, e: &mut ::gobwire::ValueEncoder<'_>) -> ::gobwire::Result<()> {
                ::gobwire::Encode::encode(&self.#member, e)
            }
        }

        #[automatically_derived]
        impl #impl_generics ::gobwire::Decode for #ident #ty_generics #decode_where {
            fn decode_into(&mut self, d: &mut ::gobwire::ValueDecoder<'_>, wire: i64) -> ::gobwire::Result<()> {
                ::gobwire::Decode::decode_into(&mut self.#member, d, wire)
            }
        }
    })
}

/// Where clauses for the three impls: the struct's own, plus the matching trait bound on every
/// type parameter.
fn param_bounds(
    input: &DeriveInput,
    where_clause: Option<&syn::WhereClause>,
) -> (syn::WhereClause, syn::WhereClause, syn::WhereClause) {
    let mut gobtype: syn::WhereClause = where_clause
        .cloned()
        .unwrap_or_else(|| syn::parse_quote!(where));
    let mut encode = gobtype.clone();
    let mut decode = gobtype.clone();
    for param in input.generics.type_params() {
        let ident = &param.ident;
        gobtype
            .predicates
            .push(syn::parse_quote!(#ident: ::gobwire::GobType));
        encode
            .predicates
            .push(syn::parse_quote!(#ident: ::gobwire::GobType + ::gobwire::Encode));
        decode
            .predicates
            .push(syn::parse_quote!(#ident: ::gobwire::Decode + ::core::default::Default));
    }
    (gobtype, encode, decode)
}
