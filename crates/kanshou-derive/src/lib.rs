//! `#[derive(Introspect)]` — auto-implements `kanshou::Introspect`
//! for a struct whose `pub` fields are each independently queryable.
//!
//! ## Default behavior
//!
//! - Every `pub` named field becomes a top-level path leaf. A query
//!   `Query { path: ["field_name"] }` returns `serde_json::to_value(&self.field_name)`.
//! - Tuple structs, unit structs, and enums are unsupported (compile
//!   error). Consumers with non-struct shapes hand-write `Introspect`.
//!
//! ## Field attributes
//!
//! Per-field `#[introspect(...)]` modifies the leaf shape:
//!
//! - `#[introspect(skip)]` — exclude from the query surface. Useful for
//!   internal channels, abort handles, or anything the operator
//!   shouldn't see.
//! - `#[introspect(load)]` — call `.load(Ordering::Relaxed)` before
//!   serializing. For `AtomicU64`/`AtomicUsize`/etc. so the wire shape
//!   is a number, not the atomic struct's debug print.
//! - `#[introspect(nested)]` — the field itself implements `Introspect`;
//!   nested path elements walk into it. Without this attribute,
//!   `query` on `["field_name", "subfield"]` returns
//!   `QueryError::UnknownField`. With it, the rest of the path
//!   recurses.
//! - `#[introspect(name = "wire_name")]` — expose the field under a
//!   different name on the wire than the Rust identifier. Mirrors
//!   `#[serde(rename = "...")]`. Lets consumer apps stabilize a public
//!   API even when the internal field shape evolves.
//!
//! ## Example
//!
//! ```ignore
//! use std::sync::atomic::AtomicU64;
//! use kanshou::Introspect;
//!
//! #[derive(Introspect)]
//! pub struct AppState {
//!     pub sessions: Vec<String>,
//!     #[introspect(load)]
//!     pub frame_count: AtomicU64,
//!     #[introspect(nested)]
//!     pub config: Config,
//!     #[introspect(skip)]
//!     internal: tokio::sync::mpsc::Sender<()>,
//! }
//!
//! #[derive(Introspect)]
//! pub struct Config {
//!     pub shell: String,
//!     pub width: u32,
//! }
//! ```

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, Data, DeriveInput, Fields, Lit, Meta};

/// Auto-implement `kanshou::Introspect` for a struct with named
/// `pub` fields. See module docs for attribute reference.
#[proc_macro_derive(Introspect, attributes(introspect))]
pub fn derive_introspect(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let Data::Struct(data) = &input.data else {
        return TokenStream::from(quote! {
            compile_error!("#[derive(Introspect)] only supports structs with named fields");
        });
    };
    let Fields::Named(fields) = &data.fields else {
        return TokenStream::from(quote! {
            compile_error!("#[derive(Introspect)] only supports structs with named fields");
        });
    };

    let mut field_arms = Vec::new();
    let mut schema_entries = Vec::new();

    for field in &fields.named {
        let attrs = parse_field_attrs(&field.attrs);
        if attrs.skip {
            continue;
        }
        let Some(field_ident) = field.ident.as_ref() else {
            continue;
        };
        let wire_name = attrs.rename.unwrap_or_else(|| field_ident.to_string());
        schema_entries.push(quote! { #wire_name });

        let read_expr = if attrs.load {
            quote! {
                ::serde_json::to_value(
                    self.#field_ident.load(::std::sync::atomic::Ordering::Relaxed)
                )
            }
        } else {
            quote! { ::serde_json::to_value(&self.#field_ident) }
        };

        let arm = if attrs.nested {
            quote! {
                #wire_name => {
                    if q.path.len() > 1 {
                        let sub = ::kanshou::Query {
                            path: q.path[1..].to_vec(),
                            args: q.args.clone(),
                        };
                        ::kanshou::Introspect::query(&self.#field_ident, &sub)
                    } else {
                        #read_expr.map_err(|e| ::kanshou::QueryError::internal(
                            format!("serialize {}: {}", #wire_name, e)
                        ))
                    }
                }
            }
        } else {
            quote! {
                #wire_name => {
                    if q.path.len() > 1 {
                        Err(::kanshou::QueryError::unknown_field(q.path.join(".")))
                    } else {
                        #read_expr.map_err(|e| ::kanshou::QueryError::internal(
                            format!("serialize {}: {}", #wire_name, e)
                        ))
                    }
                }
            }
        };
        field_arms.push(arm);
    }

    let expanded = quote! {
        impl #impl_generics ::kanshou::Introspect for #name #ty_generics #where_clause {
            fn query(&self, q: &::kanshou::Query) -> ::kanshou::QueryResult {
                let Some(first) = q.path.first().map(::std::string::String::as_str) else {
                    return Err(::kanshou::QueryError::unknown_field(
                        ::std::string::String::new(),
                    ));
                };
                match first {
                    #(#field_arms)*
                    other => Err(::kanshou::QueryError::unknown_field(other.to_string())),
                }
            }

            fn schema(&self) -> &'static [&'static str] {
                &[#(#schema_entries),*]
            }
        }
    };

    TokenStream::from(expanded)
}

#[derive(Default)]
struct FieldAttrs {
    skip: bool,
    load: bool,
    nested: bool,
    rename: Option<String>,
}

fn parse_field_attrs(attrs: &[syn::Attribute]) -> FieldAttrs {
    let mut out = FieldAttrs::default();
    for attr in attrs {
        if !attr.path().is_ident("introspect") {
            continue;
        }
        let Meta::List(list) = &attr.meta else { continue };
        let _ = list.parse_nested_meta(|meta| {
            let Some(ident) = meta.path.get_ident() else {
                return Ok(());
            };
            match ident.to_string().as_str() {
                "skip" => out.skip = true,
                "load" => out.load = true,
                "nested" => out.nested = true,
                "name" => {
                    let value: Lit = meta.value()?.parse()?;
                    if let Lit::Str(s) = value {
                        out.rename = Some(s.value());
                    }
                }
                _ => {}
            }
            Ok(())
        });
    }
    out
}
