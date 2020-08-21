use darling::{FromMeta, FromDeriveInput};
use proc_macro::TokenStream;
use syn::{AttributeArgs, parse_macro_input};

use nimiq_jsonrpc_server::Dispatcher;


/// Parses `#[jsonrpc(...)]`
#[derive(Default, FromMeta)]
#[darling(default)]
struct JsonRpcMeta {
    #[darling(rename = "sit")]
    ipsum: bool,
    dolor: Option<String>,
}

/// Parses derive macro call: `#[derive(Dispatcher)]`
#[derive(FromDeriveInput)]
#[darling(from_ident, attributes(jsonrpc), forward_attrs(allow, doc, cfg, async_trait), supports(struct_any))]
struct JsonRpcTraitOptions {
    ident: syn::Ident,
    attrs: Vec<syn::Attribute>,
    jsonrpc: JsonRpcMeta,
}



#[proc_macro_derive(JsonRpc)]
pub fn hello_macro_derive(input: TokenStream) -> TokenStream {
    let attr_args = parse_macro_input!(args as AttributeArgs);

    let ast = syn::parse(input).unwrap();

    // Build the trait implementation
    impl_hello_macro(&ast)
}

