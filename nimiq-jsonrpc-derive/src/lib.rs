mod service;
mod proxy;

use syn::{FnArg, Pat, Ident, Type, Signature, Attribute};
use quote::{quote, format_ident};
use proc_macro2::{TokenStream, Literal};
use heck::{CamelCase, KebabCase, MixedCase, ShoutySnakeCase, SnakeCase};
use darling::FromMeta;

use service::service_macro;
use proxy::proxy_macro;
use std::str::FromStr;


#[proc_macro_attribute]
pub fn service(args: proc_macro::TokenStream, input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    service_macro(args, input)
}

#[proc_macro_attribute]
pub fn proxy(args: proc_macro::TokenStream, input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    proxy_macro(args, input)
}


#[derive(Clone, Debug, Default)]
struct MethodAttributes {
    stream: Option<Attribute>,
}

impl MethodAttributes {
    pub fn parse(input: &mut Vec<Attribute>) -> MethodAttributes {
        let mut attrs = MethodAttributes::default();

        input.retain(|attr: &Attribute| {
            if attr.path.is_ident("stream") {
                attrs.stream = Some(attr.clone());
                false
            }
            else {
                true
            }
        });

        attrs
    }
}


pub(crate) struct RpcMethod<'a> {
    signature: &'a Signature,
    args: Vec<(&'a Ident, &'a Type)>,
    method_name_literal: Literal,
    args_struct_ident: Ident,
    attrs: MethodAttributes,
}


impl<'a> RpcMethod<'a> {
    pub fn new(signature: &'a Signature, args_struct_prefix: &'a str, attrs: &'a mut Vec<Attribute>, rename_all: &Option<RenameAll>) -> Self {
        let mut has_self = false;
        let mut args = vec![];

        for arg in &signature.inputs {
            match arg {
                FnArg::Receiver(_) => {
                    has_self = true;
                },
                FnArg::Typed(pat_type) => {
                    let ident = match &*pat_type.pat {
                        Pat::Ident(ty) => &ty.ident,
                        _ => panic!("Arguments must not be patterns."),
                    };
                    args.push((ident, &*pat_type.ty));
                },
            }
        }

        if !has_self {
            panic!("Method signature doesn't take self");
        }

        let attrs = MethodAttributes::parse(attrs);
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
            method_name_literal,
            args_struct_ident,
            attrs,
        }
    }

    pub fn generate_args_struct(&self) -> TokenStream {
        let struct_fields = self.args.iter()
            .map(|(ident, ty)| quote! { #ident: #ty, })
            .collect::<Vec<TokenStream>>();
        let args_struct_ident = &self.args_struct_ident;

        let tokens = quote! {
            #[derive(Debug, Serialize, Deserialize)]
            #[allow(non_camel_case_types)]
            struct #args_struct_ident {
                #(#struct_fields)*
            }
        };

        //println!("struct tokens: {}", tokens);

        tokens
    }

    pub fn generate_dispatcher_match_arm(&self) -> TokenStream {
        let method_args = self.args
            .iter()
            .map(|(ident, _)| quote! { params.#ident })
            .collect::<Vec<TokenStream>>();
        let args_struct_ident = &self.args_struct_ident;
        let method_ident = &self.signature.ident;
        let method_name_literal = &self.method_name_literal;

        if self.attrs.stream.is_some() {
            quote! {
                #method_name_literal => {
                    if let Some(tx) = tx {
                        return ::nimiq_jsonrpc_server::dispatch_method_with_args(
                            request,
                            move |params: #args_struct_ident| async move {
                                let stream = self.#method_ident(#(#method_args),*).await?;

                                // TODO: Take the method name from the attribute
                                let subscription = ::nimiq_jsonrpc_server::connect_stream(stream, tx, stream_id, "subscription".to_owned());

                                Ok::<_, ::nimiq_jsonrpc_core::RpcError>(subscription)
                            }
                        ).await
                    }
                    else {
                        let ::nimiq_jsonrpc_core::Request { id, .. } = request;
                            ::nimiq_jsonrpc_server::error_response(
                            id,
                            || ::nimiq_jsonrpc_core::RpcError::internal_error(Some("Client does not support streams".to_owned()))
                        )
                    }
                }
            }
        }
        else {
            quote! {
                #method_name_literal => {
                    return ::nimiq_jsonrpc_server::dispatch_method_with_args(
                        request,
                        move |params: #args_struct_ident| async move {
                            Ok::<_, ::nimiq_jsonrpc_core::RpcError>(self.#method_ident(#(#method_args),*).await?)
                        }
                    ).await
                }
            }
        }
    }

    pub fn generate_dispatcher_method_matcher(&self) -> TokenStream {
        let method_name_literal = &self.method_name_literal;

        quote!{ #method_name_literal => true, }
    }

    pub fn generate_proxy_method(&self) -> TokenStream {
        let method_ident = &self.signature.ident;
        let args_struct_ident = &self.args_struct_ident;
        let method_name_literal = &self.method_name_literal;
        let output = &self.signature.output;

        let method_args = self.args.iter()
            .map(|(ident, ty)| quote! { #ident: #ty })
            .collect::<Vec<TokenStream>>();

        let struct_fields = self.args.iter()
            .map(|(ident, _)| quote! { #ident })
            .collect::<Vec<TokenStream>>();


        let transform_return_value = if self.attrs.stream.is_some() {
            quote! {
                let return_value = self.client.connect_stream(return_value).await;
            }
        }
        else {
            quote! {}
        };

        quote! {
            async fn #method_ident(&mut self, #(#method_args),*) #output {
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
    CamelCase,
    KebabCase,
    MixedCase,
    ShoutySnakeCase,
    SnakeCase,
}

impl FromStr for RenameAll {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "CamelCase" => Self::CamelCase,
            "kebab-case" => Self::KebabCase,
            "mixedCase" => Self::MixedCase,
            "SHOUTY_SNAKE_CASE" => Self::ShoutySnakeCase,
            "snake_case" => Self::SnakeCase,
            _ => panic!("Invalid case name: {}", s),
        })
    }
}

impl RenameAll {
    pub fn rename(&self, name: &str) -> String {
        match self {
            RenameAll::CamelCase => name.to_camel_case(),
            RenameAll::KebabCase => name.to_kebab_case(),
            RenameAll::MixedCase => name.to_mixed_case(),
            RenameAll::ShoutySnakeCase => name.to_shouty_snake_case(),
            RenameAll::SnakeCase => name.to_snake_case(),
        }
    }
}