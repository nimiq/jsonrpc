use darling::FromMeta;
use proc_macro2::TokenStream;
use quote::quote;
use syn::{parse_macro_input, AttributeArgs, ImplItem, ItemImpl, Type};

use crate::{RenameAll, RpcMethod};

/// Parses `#[service(...)]`
#[derive(Clone, Debug, Default, FromMeta)]
#[darling(default)]
struct ServiceMeta {
    rename_all: Option<String>,
}

pub fn service_macro(
    args: proc_macro::TokenStream,
    input: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let attr_args = parse_macro_input!(args as AttributeArgs);
    let mut im = parse_macro_input!(input as ItemImpl);

    let args = match ServiceMeta::from_list(&attr_args) {
        Ok(v) => v,
        Err(e) => {
            return proc_macro::TokenStream::from(e.write_errors());
        }
    };

    //println!("args: {:#?}", args);

    let service_impl = impl_service(&mut im, &args);

    //println!("impl: {}", quote!{#im});
    //println!("service impl: {}", service_impl);

    proc_macro::TokenStream::from(quote! {
        #im
        #service_impl
    })
}

fn impl_service(im: &mut ItemImpl, args: &ServiceMeta) -> TokenStream {
    let mut args_structs = vec![];
    let mut match_arms = vec![];
    let mut name_match_arms = vec![];
    let mut method_names = vec![];

    let struct_path = match &*im.self_ty {
        Type::Path(path) => &path.path,
        _ => panic!("Can't implement JSON RPC service for type"),
    };

    let struct_name = &struct_path.segments.last().unwrap().ident;
    let args_struct_prefix = format!("ServiceArgs_{}", struct_name);

    let rename_all: Option<RenameAll> = args.rename_all.as_ref().map(|r| r.parse().unwrap());

    for item in &mut im.items {
        if let ImplItem::Method(method) = item {
            let method = RpcMethod::new(
                &method.sig,
                &args_struct_prefix,
                &mut method.attrs,
                &rename_all,
            );

            let match_arm = method.generate_dispatcher_match_arm();
            let method_name_lit = &method.method_name_literal;

            //println!("Generated match arm:");
            //println!("{}", match_arm);

            args_structs.push(method.generate_args_struct());
            match_arms.push(match_arm);
            name_match_arms.push(method.generate_dispatcher_method_matcher());
            method_names.push(quote! { #method_name_lit });
        }
    }

    quote! {
        #(#args_structs)*

        #[::async_trait::async_trait]
        impl ::nimiq_jsonrpc_server::Dispatcher for #struct_path {
            async fn dispatch(
                &mut self,
                request: ::nimiq_jsonrpc_core::Request,
                tx: Option<&::tokio::sync::mpsc::Sender<::std::vec::Vec<u8>>>,
                stream_id: u64,
            ) -> Option<::nimiq_jsonrpc_core::Response> {
                match request.method.as_str() {
                    #(#match_arms)*
                    _ => ::nimiq_jsonrpc_server::method_not_found(request),
                }
            }

            fn match_method(&self, name: &str) -> bool {
                match name {
                    #(#name_match_arms)*
                    _ => false,
                }
            }

            fn method_names(&self) -> Vec<&str> {
                vec![
                    #(#method_names),*
                ]
            }
        }
    }
}
