extern crate proc_macro;

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;

#[derive(Default)]
struct Configuration {
    parallelism: Option<usize>,
}

impl Configuration {
    fn set_parallelism(&mut self, span: Span, lit: syn::Lit) -> Result<(), syn::Error> {
        if self.parallelism.is_some() {
            return Err(syn::Error::new(span, "parallelism already set"));
        }
        if let syn::Lit::Int(lit) = lit {
            let parallelism = lit.base10_parse::<usize>()?;
            if parallelism > 0 {
                self.parallelism = Some(parallelism);
                return Ok(());
            }
        }
        Err(syn::Error::new(span, "parallelism should be positive integer"))
    }
}

fn parse_config(args: syn::AttributeArgs) -> Result<Configuration, syn::Error> {
    let mut config = Configuration::default();
    for arg in args.into_iter() {
        match arg {
            syn::NestedMeta::Meta(syn::Meta::NameValue(name_value)) => {
                let name = name_value
                    .path
                    .get_ident()
                    .ok_or_else(|| syn::Error::new_spanned(&name_value, "invalid attribute name"))?
                    .to_string();
                match name.as_str() {
                    "parallelism" => config.set_parallelism(name_value.lit.span(), name_value.lit)?,
                    _ => return Err(syn::Error::new_spanned(&name_value, "unknown attribute name")),
                }
            },
            _ => return Err(syn::Error::new_spanned(arg, "unknown attribute")),
        }
    }
    Ok(config)
}

fn generate(is_test: bool, attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = syn::parse_macro_input!(attr as syn::AttributeArgs);
    let config = parse_config(args).unwrap();
    let input = syn::parse_macro_input!(item as syn::ItemFn);

    let ret = &input.sig.output;
    let inputs = &input.sig.inputs;
    let name = &input.sig.ident;
    let body = &input.block;
    let attrs = &input.attrs;
    let vis = &input.vis;

    let macro_name = if is_test { "#[stuck::test]" } else { "#[stuck::main]" };

    if input.sig.asyncness.is_some() {
        let err =
            syn::Error::new_spanned(input, format!("only synchronous function can be tagged with {}", macro_name));
        return TokenStream::from(err.into_compile_error());
    }

    if !is_test && name != "main" {
        let err = syn::Error::new_spanned(name, "only the main function can be tagged with #[stuck::main]");
        return TokenStream::from(err.into_compile_error());
    }

    let header = if is_test {
        quote! {
            #[::core::prelude::v1::test]
        }
    } else {
        quote! {}
    };

    let parallelism = config.parallelism.unwrap_or(0);
    let result = quote! {
        #header
        #vis fn #name() #ret {
            #(#attrs)*
            fn entry(#inputs) #ret {
                #body
            }

            let mut builder = stuck::runtime::Builder::default();
            if #parallelism != 0 {
                builder.parallelism(#parallelism);
            }
            let runtime = builder.build();
            let task = runtime.spawn(entry);
            task.join().unwrap()
        }
    };

    result.into()
}

#[cfg(not(test))]
#[proc_macro_attribute]
pub fn main(attr: TokenStream, item: TokenStream) -> TokenStream {
    generate(false, attr, item)
}

#[proc_macro_attribute]
pub fn test(attr: TokenStream, item: TokenStream) -> TokenStream {
    generate(true, attr, item)
}
