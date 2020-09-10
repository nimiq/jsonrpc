use darling::FromMeta;
use proc_macro2::TokenStream;
use syn::{parse_macro_input, AttributeArgs, Type, ItemImpl, ImplItem};
use quote::quote;

use crate::RpcMethod;


/// Parses `#[service(...)]`
#[derive(Clone, Debug, Default, FromMeta)]
#[darling(default)]
struct ServiceMeta {
}


pub fn service_macro(args: proc_macro::TokenStream, input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let attr_args = parse_macro_input!(args as AttributeArgs);
    let im = parse_macro_input!(input as ItemImpl);

    let args = match ServiceMeta::from_list(&attr_args) {
        Ok(v) => v,
        Err(e) => { return proc_macro::TokenStream::from(e.write_errors()); }
    };

    //println!("args: {:#?}", args);

    let service_impl = impl_service(&im, &args);

    //println!("impl: {}", quote!{#im});
    //println!("service impl: {}", service_impl);

    proc_macro::TokenStream::from(quote! {
        #im
        #service_impl
    })
}


fn impl_service(im: &ItemImpl, _args: &ServiceMeta) -> TokenStream {
    let mut args_structs = vec![];
    let mut match_arms = vec![];

    let struct_path = match &*im.self_ty {
        Type::Path(path) => &path.path,
        _ => panic!("Can't implement JSON RPC service for type"),
    };

    let struct_name = &struct_path.segments.last().unwrap().ident;
    let args_struct_prefix = format!("ServiceArgs_{}", struct_name);

    for item in &im.items {
        if let ImplItem::Method(method) = item {
            let method = RpcMethod::new(&method.sig, &args_struct_prefix);

            args_structs.push(method.generate_args_struct());
            match_arms.push(method.generate_dispatcher_match_arm());
        }
    }

    quote! {
        #(#args_structs)*

        #[::async_trait::async_trait]
        impl ::nimiq_jsonrpc_server::Dispatcher for #struct_path {
            async fn dispatch(&mut self, request: ::nimiq_jsonrpc_core::Request) -> Option<::nimiq_jsonrpc_core::Response> {
                match request.method.as_str() {
                    #(#match_arms)*

                    _ => {
                        let ::nimiq_jsonrpc_core::Request { id, method, .. } = request;

                        ::nimiq_jsonrpc_server::error_response(
                            id,
                            || ::nimiq_jsonrpc_core::RpcError::method_not_found(Some(format!("Method does not exist: {}", method)))
                        )
                    }
                }
            }
        }
    }
}
