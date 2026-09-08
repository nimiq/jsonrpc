mod proxy;
mod service;

use darling::FromMeta;
use heck::{ToKebabCase, ToLowerCamelCase, ToShoutySnakeCase, ToSnakeCase, ToUpperCamelCase};
use proc_macro2::{Literal, TokenStream};
use quote::{format_ident, quote};
use syn::{Attribute, FnArg, Ident, Pat, Signature, Type};

use proxy::proxy_macro;
use service::service_macro;
use std::str::FromStr;

#[proc_macro_attribute]
pub fn service(
    args: proc_macro::TokenStream,
    input: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    service_macro(args, input)
}

#[proc_macro_attribute]
pub fn proxy(
    args: proc_macro::TokenStream,
    input: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    proxy_macro(args, input)
}

#[derive(Clone, Debug, Default)]
struct MethodAttributes {
    stream: Option<Attribute>,
    deprecated: Option<Deprecation>,
}

/// Information extracted from a `#[deprecated]` attribute on an RPC method.
#[derive(Clone, Debug, Default)]
struct Deprecation {
    /// The optional note (e.g. `#[deprecated = "use foo instead"]` or
    /// `#[deprecated(note = "...")]`).
    note: Option<String>,
    /// The optional version (`#[deprecated(since = "...")]`).
    since: Option<String>,
}

impl MethodAttributes {
    pub fn parse(
        input: &mut Vec<Attribute>,
        strip_deprecated: bool,
        method_ident: &Ident,
    ) -> MethodAttributes {
        let mut attrs = MethodAttributes::default();

        input.retain(|attr: &Attribute| {
            if attr.path().is_ident("stream") {
                attrs.stream = Some(attr.clone());
                false
            } else if attr.path().is_ident("deprecated") {
                // rustc rejects duplicate `#[deprecated]` attributes itself when they are kept;
                // when stripping, this check is the only one left.
                if strip_deprecated && attrs.deprecated.is_some() {
                    panic!(
                        "Duplicate #[deprecated] attribute on method `{}`",
                        method_ident
                    );
                }
                let (deprecation, error) = parse_deprecation(attr);
                if let Some(error) = error {
                    // If the attribute stays in place, rustc reports the error with proper
                    // spans, and whatever metadata did parse is kept. When stripping, rustc
                    // never sees the attribute, so the macro is the only validation left.
                    if strip_deprecated {
                        panic!(
                            "Invalid #[deprecated] attribute on method `{}`: {}",
                            method_ident, error
                        );
                    }
                }
                attrs.deprecated = Some(deprecation);
                // Rust rejects `#[deprecated]` on the methods of a trait `impl`
                // (`useless_deprecated`), so it must be stripped there. Everywhere else it is
                // kept, so that Rust callers still get a compile-time deprecation warning.
                !strip_deprecated
            } else {
                true
            }
        });

        attrs
    }
}

/// Extracts the optional `note` and `since` from a `#[deprecated]` attribute in any of its
/// supported forms: `#[deprecated]`, `#[deprecated = "..."]`, or
/// `#[deprecated(note = "...", since = "...")]`.
///
/// Always returns the metadata that could be extracted, alongside the first error encountered (if
/// any), so that a single bad key doesn't throw away the valid ones.
fn parse_deprecation(attr: &Attribute) -> (Deprecation, Option<syn::Error>) {
    let mut deprecation = Deprecation::default();
    let mut error = None;

    match &attr.meta {
        syn::Meta::NameValue(nv) => match &nv.value {
            syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) => deprecation.note = Some(s.value()),
            syn::Expr::Lit(_) => {
                error = Some(syn::Error::new_spanned(
                    &nv.value,
                    "expected a string literal",
                ));
            }
            // Non-literal expressions (e.g. `concat!(...)`) are valid here — rustc expands them —
            // but can't be evaluated at macro expansion time, so the note just doesn't make it
            // into the runtime metadata.
            _ => {}
        },
        syn::Meta::List(_) => {
            let result = attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("note") {
                    deprecation.note = Some(meta.value()?.parse::<syn::LitStr>()?.value());
                } else if meta.path.is_ident("since") {
                    deprecation.since = Some(meta.value()?.parse::<syn::LitStr>()?.value());
                } else {
                    // The value must be consumed even if the key is unusable — leaving it
                    // unparsed would abort `parse_nested_meta` and lose every key after it.
                    if meta.input.peek(syn::Token![=]) {
                        meta.value()?.parse::<syn::Expr>()?;
                    }
                    // `suggestion` is rustc's (currently unstable) third key; anything else is
                    // a typo.
                    if !meta.path.is_ident("suggestion") && error.is_none() {
                        error = Some(meta.error("expected `note` or `since`"));
                    }
                }
                Ok(())
            });
            if error.is_none() {
                error = result.err();
            }
        }
        syn::Meta::Path(_) => {}
    }

    (deprecation, error)
}

/// Quotes an `Option<String>` as an `Option<&str>` expression.
fn quote_option_str(value: &Option<String>) -> TokenStream {
    match value {
        Some(value) => {
            let lit = Literal::string(value);
            quote! { Some(#lit) }
        }
        None => quote! { None },
    }
}

pub(crate) struct RpcMethod<'a> {
    signature: &'a Signature,
    args: Vec<(&'a Ident, &'a Type)>,
    method_name: String,
    method_name_literal: Literal,
    args_struct_ident: Ident,
    attrs: MethodAttributes,
}

impl<'a> RpcMethod<'a> {
    pub fn new(
        signature: &'a Signature,
        args_struct_prefix: &'a str,
        attrs: &'a mut Vec<Attribute>,
        rename_all: &Option<RenameAll>,
        strip_deprecated: bool,
    ) -> Self {
        let mut has_self = false;
        let mut args = vec![];

        for arg in &signature.inputs {
            match arg {
                FnArg::Receiver(_) => {
                    has_self = true;
                }
                FnArg::Typed(pat_type) => {
                    let ident = match &*pat_type.pat {
                        Pat::Ident(ty) => &ty.ident,
                        _ => panic!("Arguments must not be patterns."),
                    };
                    args.push((ident, &*pat_type.ty));
                }
            }
        }

        if !has_self {
            panic!("Method signature doesn't take self");
        }

        let attrs = MethodAttributes::parse(attrs, strip_deprecated, &signature.ident);
        //println!("Method attributes: {:?}", attrs);

        let method_name = signature.ident.to_string();
        let method_name = rename_all
            .as_ref()
            .map(|r| r.rename(&method_name))
            .unwrap_or(method_name);
        let method_name_literal = Literal::string(&method_name);

        let args_struct_ident = format_ident!("{}_{}", args_struct_prefix, signature.ident);

        Self {
            signature,
            args,
            method_name,
            method_name_literal,
            args_struct_ident,
            attrs,
        }
    }

    pub fn generate_args_struct(&self) -> TokenStream {
        let struct_fields = self
            .args
            .iter()
            .map(|(ident, ty)| quote! { #ident: #ty, })
            .collect::<Vec<TokenStream>>();
        let args_struct_ident = &self.args_struct_ident;

        let tokens = quote! {
            #[derive(Debug, ::serde::Serialize, ::serde::Deserialize)]
            #[allow(non_camel_case_types)]
            struct #args_struct_ident {
                #(#struct_fields)*
            }
        };

        //println!("struct tokens: {}", tokens);

        tokens
    }

    /// Returns `true` if this method is marked with `#[deprecated]`.
    pub fn is_deprecated(&self) -> bool {
        self.attrs.deprecated.is_some()
    }

    /// Generates a `MethodDeprecation` struct literal with this method's deprecation metadata, or
    /// `None` if the method isn't deprecated.
    pub fn generate_deprecation_entry(&self) -> Option<TokenStream> {
        self.attrs.deprecated.as_ref().map(|deprecation| {
            let method_name_literal = &self.method_name_literal;
            let note = quote_option_str(&deprecation.note);
            let since = quote_option_str(&deprecation.since);
            quote! {
                ::nimiq_jsonrpc_server::MethodDeprecation {
                    method: #method_name_literal,
                    note: #note,
                    since: #since,
                }
            }
        })
    }

    /// Generates the statement that logs a warning when a deprecated method is dispatched. Expands
    /// to nothing for methods that aren't deprecated.
    fn generate_deprecation_warning(&self) -> TokenStream {
        match self.generate_deprecation_entry() {
            Some(entry) => quote! { ::nimiq_jsonrpc_server::log_deprecated(&#entry); },
            None => quote! {},
        }
    }

    pub fn generate_dispatcher_match_arm(&self) -> TokenStream {
        let method_args = self
            .args
            .iter()
            .map(|(ident, _)| quote! { params.#ident })
            .collect::<Vec<TokenStream>>();
        let args_struct_ident = &self.args_struct_ident;
        let method_ident = &self.signature.ident;
        let method_name = &self.method_name;
        let method_name_literal = &self.method_name_literal;
        let deprecation_warning = self.generate_deprecation_warning();

        if self.attrs.stream.is_some() {
            quote! {
                #method_name_literal => {
                    #deprecation_warning
                    if let Some(tx) = tx {
                        return ::nimiq_jsonrpc_server::dispatch_method_with_args(
                            request,
                            move |params: #args_struct_ident| async move {
                                let stream = self.#method_ident(#(#method_args),*).await?;
                                let notifier = ::std::sync::Arc::new(::nimiq_jsonrpc_server::Notify::new());
                                let listener = notifier.clone();

                                let subscription = ::nimiq_jsonrpc_server::connect_stream(stream, tx, stream_id, #method_name.to_owned(), listener, frame_type);

                                Ok::<_, ::nimiq_jsonrpc_core::RpcError>((subscription, Some(notifier)))
                            }
                        ).await
                    }
                    else {
                        let ::nimiq_jsonrpc_core::Request { id, .. } = request;
                            ::nimiq_jsonrpc_server::error_response(
                            id,
                            || ::nimiq_jsonrpc_core::RpcError::internal_from_string(Some("Client does not support streams".to_owned()))
                        )
                    }
                }
            }
        } else {
            quote! {
                #method_name_literal => {
                    #deprecation_warning
                    return ::nimiq_jsonrpc_server::dispatch_method_with_args(
                        request,
                        move |params: #args_struct_ident| async move {
                            Ok::<(_, Option<::std::sync::Arc<::nimiq_jsonrpc_server::Notify>>), ::nimiq_jsonrpc_core::RpcError>((self.#method_ident(#(#method_args),*).await?, None))
                        }
                    ).await
                }
            }
        }
    }

    pub fn generate_dispatcher_method_matcher(&self) -> TokenStream {
        let method_name_literal = &self.method_name_literal;

        quote! { #method_name_literal => true, }
    }

    pub fn generate_proxy_method(&self) -> TokenStream {
        let method_ident = &self.signature.ident;
        let args_struct_ident = &self.args_struct_ident;
        let method_name_literal = &self.method_name_literal;
        let output = &self.signature.output;
        //println!("Generating proxy method: {}", method_ident);

        let method_args = self
            .args
            .iter()
            .map(|(ident, ty)| quote! { #ident: #ty })
            .collect::<Vec<TokenStream>>();

        let struct_fields = self
            .args
            .iter()
            .map(|(ident, _)| quote! { #ident })
            .collect::<Vec<TokenStream>>();

        let transform_return_value = if self.attrs.stream.is_some() {
            quote! {
                let return_value = self.client.connect_stream(return_value).await;
            }
        } else {
            quote! {}
        };

        quote! {
            async fn #method_ident(&self, #(#method_args),*) #output {
                let args = #args_struct_ident {
                    #(#struct_fields),*
                };
                let return_value = self.client.send_request(
                    #method_name_literal,
                    &args,
                ).await?;

                #transform_return_value

                Ok(return_value)
            }
        }
    }
}

#[derive(Clone, Debug, FromMeta)]
pub(crate) enum RenameAll {
    Camel,
    Kebab,
    Mixed,
    ShoutySnake,
    Snake,
}

impl FromStr for RenameAll {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "CamelCase" => Self::Camel,
            "kebab-case" => Self::Kebab,
            "mixedCase" | "camelCase" => Self::Mixed,
            "SHOUTY_SNAKE_CASE" => Self::ShoutySnake,
            "snake_case" => Self::Snake,
            _ => panic!("Invalid case name: {}", s),
        })
    }
}

impl RenameAll {
    pub fn rename(&self, name: &str) -> String {
        match self {
            RenameAll::Camel => name.to_upper_camel_case(),
            RenameAll::Kebab => name.to_kebab_case(),
            RenameAll::Mixed => name.to_lower_camel_case(),
            RenameAll::ShoutySnake => name.to_shouty_snake_case(),
            RenameAll::Snake => name.to_snake_case(),
        }
    }
}

#[cfg(test)]
mod tests {
    use syn::parse_quote;

    use super::*;

    fn parse(attr: Attribute) -> (Deprecation, Option<syn::Error>) {
        parse_deprecation(&attr)
    }

    fn parse_ok(attr: Attribute) -> Deprecation {
        let (deprecation, error) = parse(attr);
        assert!(error.is_none(), "unexpected error: {:?}", error);
        deprecation
    }

    #[test]
    fn it_parses_all_deprecated_forms() {
        let deprecation = parse_ok(parse_quote!(#[deprecated]));
        assert_eq!(deprecation.note, None);
        assert_eq!(deprecation.since, None);

        let deprecation = parse_ok(parse_quote!(#[deprecated()]));
        assert_eq!(deprecation.note, None);
        assert_eq!(deprecation.since, None);

        let deprecation = parse_ok(parse_quote!(#[deprecated = "use foo"]));
        assert_eq!(deprecation.note.as_deref(), Some("use foo"));
        assert_eq!(deprecation.since, None);

        let deprecation = parse_ok(parse_quote!(#[deprecated(note = "use foo")]));
        assert_eq!(deprecation.note.as_deref(), Some("use foo"));
        assert_eq!(deprecation.since, None);

        let deprecation = parse_ok(parse_quote!(#[deprecated(since = "1.0")]));
        assert_eq!(deprecation.note, None);
        assert_eq!(deprecation.since.as_deref(), Some("1.0"));
    }

    #[test]
    fn it_parses_note_and_since_in_any_order() {
        let deprecation = parse_ok(parse_quote!(#[deprecated(note = "use foo", since = "1.0")]));
        assert_eq!(deprecation.note.as_deref(), Some("use foo"));
        assert_eq!(deprecation.since.as_deref(), Some("1.0"));

        // Regression: `since` coming first used to abort parsing and lose the `note`.
        let deprecation = parse_ok(parse_quote!(#[deprecated(since = "1.0", note = "use foo")]));
        assert_eq!(deprecation.note.as_deref(), Some("use foo"));
        assert_eq!(deprecation.since.as_deref(), Some("1.0"));
    }

    #[test]
    fn it_rejects_invalid_deprecated_attributes() {
        assert!(parse(parse_quote!(#[deprecated(reason = "use foo")]))
            .1
            .is_some());
        assert!(parse(parse_quote!(#[deprecated = 42])).1.is_some());
    }

    #[test]
    fn it_keeps_metadata_parsed_around_an_invalid_key() {
        let (deprecation, error) =
            parse(parse_quote!(#[deprecated(since = "1.0", reason = "x", note = "use foo")]));
        assert!(error.is_some());
        assert_eq!(deprecation.note.as_deref(), Some("use foo"));
        assert_eq!(deprecation.since.as_deref(), Some("1.0"));
    }

    #[test]
    fn it_ignores_the_unstable_suggestion_key() {
        let deprecation =
            parse_ok(parse_quote!(#[deprecated(suggestion = "foo", note = "use foo")]));
        assert_eq!(deprecation.note.as_deref(), Some("use foo"));
        assert_eq!(deprecation.since, None);
    }

    #[test]
    fn it_tolerates_non_literal_notes() {
        // `#[deprecated = concat!(...)]` is valid Rust (rustc expands the macro); the note just
        // can't be known at macro expansion time.
        let deprecation = parse_ok(parse_quote!(#[deprecated = concat!("use ", "foo")]));
        assert_eq!(deprecation.note, None);
    }

    #[test]
    fn it_keeps_or_strips_the_deprecated_attribute() {
        let ident: Ident = parse_quote!(dummy);

        let mut attrs: Vec<Attribute> = vec![parse_quote!(#[deprecated = "use foo"])];
        let parsed = MethodAttributes::parse(&mut attrs, false, &ident);
        assert!(parsed.deprecated.is_some());
        assert_eq!(
            attrs.len(),
            1,
            "must stay in place for compile-time warnings"
        );

        let mut attrs: Vec<Attribute> = vec![parse_quote!(#[deprecated = "use foo"])];
        let parsed = MethodAttributes::parse(&mut attrs, true, &ident);
        assert!(parsed.deprecated.is_some());
        assert!(attrs.is_empty(), "must be stripped in trait impls");
    }

    #[test]
    #[should_panic(expected = "Duplicate #[deprecated] attribute")]
    fn it_rejects_duplicate_deprecated_attributes_when_stripping() {
        let ident: Ident = parse_quote!(dummy);
        let mut attrs: Vec<Attribute> = vec![
            parse_quote!(#[deprecated = "first"]),
            parse_quote!(#[deprecated = "second"]),
        ];
        MethodAttributes::parse(&mut attrs, true, &ident);
    }
}
