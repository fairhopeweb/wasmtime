use crate::config::Asyncness;
use crate::funcs::func_bounds;
use crate::names;
use crate::CodegenSettings;
use proc_macro2::{Ident, Span, TokenStream};
use quote::{format_ident, quote};
use std::collections::HashSet;

pub fn link_module(
    module: &witx::Module,
    target_path: Option<&syn::Path>,
    settings: &CodegenSettings,
) -> TokenStream {
    let module_ident = names::module(&module.name);

    let send_bound = if settings.async_.contains_async(module) {
        quote! { + Send, T: Send }
    } else {
        quote! {}
    };

    let mut bodies = Vec::new();
    let mut bounds = HashSet::new();
    for f in module.funcs() {
        let asyncness = settings.async_.get(module.name.as_str(), f.name.as_str());
        bodies.push(generate_func(&module, &f, target_path, asyncness));
        let bound = func_bounds(module, &f, settings);
        for b in bound {
            bounds.insert(b);
        }
    }

    let ctx_bound = if let Some(target_path) = target_path {
        let bounds = bounds
            .into_iter()
            .map(|b| quote!(#target_path::#module_ident::#b));
        quote!( #(#bounds)+* #send_bound )
    } else {
        let bounds = bounds.into_iter();
        quote!( #(#bounds)+* #send_bound )
    };

    let func_name = if target_path.is_none() {
        format_ident!("add_to_linker")
    } else {
        format_ident!("add_{}_to_linker", module_ident)
    };

    quote! {
        /// Adds all instance items to the specified `Linker`.
        pub fn #func_name<T, U>(
            linker: &mut wiggle::wasmtime_crate::Linker<T>,
            get_cx: impl Fn(&mut T) -> &mut U + Send + Sync + Copy + 'static,
        ) -> wiggle::anyhow::Result<()>
            where
                U: #ctx_bound #send_bound
        {
            #(#bodies)*
            Ok(())
        }
    }
}

fn generate_func(
    module: &witx::Module,
    func: &witx::InterfaceFunc,
    target_path: Option<&syn::Path>,
    asyncness: Asyncness,
) -> TokenStream {
    let module_str = module.name.as_str();
    let module_ident = names::module(&module.name);

    let field_str = func.name.as_str();
    let field_ident = names::func(&func.name);

    let (params, results) = func.wasm_signature();

    let arg_names = (0..params.len())
        .map(|i| Ident::new(&format!("arg{}", i), Span::call_site()))
        .collect::<Vec<_>>();
    let arg_decls = params
        .iter()
        .enumerate()
        .map(|(i, ty)| {
            let name = &arg_names[i];
            let wasm = names::wasm_type(*ty);
            quote! { #name: #wasm }
        })
        .collect::<Vec<_>>();

    let ret_ty = match results.len() {
        0 => quote!(()),
        1 => names::wasm_type(results[0]),
        _ => unimplemented!(),
    };

    let await_ = if asyncness.is_sync() {
        quote!()
    } else {
        quote!(.await)
    };

    let abi_func = if let Some(target_path) = target_path {
        quote!( #target_path::#module_ident::#field_ident )
    } else {
        quote!( #field_ident )
    };

    let body = quote! {
        let export = caller.get_export("memory");
        let (mem, ctx) = match &export {
            Some(wiggle::wasmtime_crate::Extern::Memory(m)) => {
                let (mem, ctx) = m.data_and_store_mut(&mut caller);
                let ctx = get_cx(ctx);
                (wiggle::wasmtime::WasmtimeGuestMemory::new(mem), ctx)
            }
            Some(wiggle::wasmtime_crate::Extern::SharedMemory(m)) => {
                let ctx = get_cx(caller.data_mut());
                (wiggle::wasmtime::WasmtimeGuestMemory::shared(m.data()), ctx)
            }
            _ => wiggle::anyhow::bail!("missing required memory export"),
        };
        Ok(<#ret_ty>::from(#abi_func(ctx, &mem #(, #arg_names)*) #await_ ?))
    };

    match asyncness {
        Asyncness::Async => {
            let wrapper = format_ident!("func_wrap{}_async", params.len());
            quote! {
                linker.#wrapper(
                    #module_str,
                    #field_str,
                    move |mut caller: wiggle::wasmtime_crate::Caller<'_, T> #(, #arg_decls)*| {
                        Box::new(async move { #body })
                    },
                )?;
            }
        }

        Asyncness::Blocking => {
            quote! {
                linker.func_wrap(
                    #module_str,
                    #field_str,
                    move |mut caller: wiggle::wasmtime_crate::Caller<'_, T> #(, #arg_decls)*| -> wiggle::anyhow::Result<#ret_ty> {
                        let result = async { #body };
                        wiggle::run_in_dummy_executor(result)?
                    },
                )?;
            }
        }

        Asyncness::Sync => {
            quote! {
                linker.func_wrap(
                    #module_str,
                    #field_str,
                    move |mut caller: wiggle::wasmtime_crate::Caller<'_, T> #(, #arg_decls)*| -> wiggle::anyhow::Result<#ret_ty> {
                        #body
                    },
                )?;
            }
        }
    }
}
