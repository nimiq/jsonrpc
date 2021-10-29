use darling::FromMeta;
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{parse_macro_input, AttributeArgs, ItemTrait, TraitItem};

use crate::{RenameAll, RpcMethod};

/// Parses `#[proxy(...)]`
#[derive(Clone, Debug, Default, FromMeta)]
#[darling(default)]
struct ProxyMeta {
    name: Option<String>,
    rename_all: Option<String>,
}

pub fn proxy_macro(
    args: proc_macro::TokenStream,
    input: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let attr_args = parse_macro_input!(args as AttributeArgs);
    let mut tr = parse_macro_input!(input as ItemTrait);

    let args = match ProxyMeta::from_list(&attr_args) {
        Ok(v) => v,
        Err(e) => {
            return proc_macro::TokenStream::from(e.write_errors());
        }
    };

    //println!("args: {:#?}", args);
    //println!("trait: {}", quote!{#tr});

    let proxy_impl = impl_service(&mut tr, &args);

    //println!("proxy impl: {}", proxy_impl);

    proc_macro::TokenStream::from(quote! {
        #tr
        #proxy_impl
    })
}

fn impl_service(tr: &mut ItemTrait, args: &ProxyMeta) -> TokenStream {
    let trait_ident = &tr.ident;

    let struct_ident = match &args.name {
        Some(name) => format_ident!("{}", name),
        None => format_ident!("{}Proxy", trait_ident),
    };
    //println!("Generating proxy for {} as {}", trait_ident, struct_ident);

    //println!("proxy struct name: {:?}", struct_ident);

    let args_struct_prefix = format!("ProxyArgs_{}", trait_ident);

    let mut args_structs = vec![];
    let mut method_impls = vec![];

    let rename_all: Option<RenameAll> = args.rename_all.as_ref().map(|r| r.parse().unwrap());

    for item in &mut tr.items {
        if let TraitItem::Method(method) = item {
            let method = RpcMethod::new(
                &method.sig,
                &args_struct_prefix,
                &mut method.attrs,
                &rename_all,
            );

            let method_code = method.generate_proxy_method();

            //println!("Generated proxy method:");
            //println!("{}", method_code);

            args_structs.push(method.generate_args_struct());
            method_impls.push(method_code);
        }
    }

    quote! {
        #(#args_structs)*

        pub struct #struct_ident<C>
            where C: ::nimiq_jsonrpc_client::Client + ::std::marker::Send + ::std::marker::Sync
        {
            pub client: C,
        }

        impl<C> #struct_ident<C>
            where C: ::nimiq_jsonrpc_client::Client + ::std::marker::Send + ::std::marker::Sync
        {
            pub fn new(client: C) -> Self {
                Self {
                    client,
                }
            }
        }

        #[::async_trait::async_trait]
        impl<C> #trait_ident for #struct_ident<C>
            where C: ::nimiq_jsonrpc_client::Client + ::std::marker::Send + ::std::marker::Sync
        {
            type Error = C::Error;

            #(#method_impls)*
        }
    }
}
