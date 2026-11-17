//! `#[derive(Snap)]`: saves every field in declaration order and loads them
//! back in place. Fields marked `#[snap(skip)]` keep their current value on
//! load (or `Default::default()` inside an enum variant that load rebuilds).

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Field, Fields, Ident, parse_macro_input};

#[proc_macro_derive(Snap, attributes(snap))]
pub fn derive_snap(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(&input) {
        Ok(t) => t.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn skipped(f: &Field) -> syn::Result<bool> {
    for a in &f.attrs {
        if a.path().is_ident("snap") {
            let what: Ident = a.parse_args()?;
            if what != "skip" {
                return Err(syn::Error::new(what.span(), "expected #[snap(skip)]"));
            }
            return Ok(true);
        }
    }
    Ok(false)
}

/// The schema of a field list: field names and types, mixed with the schema
/// of every saved field's type.
fn schema(name: &str, fields: &Fields) -> syn::Result<TokenStream> {
    let mut text = name.to_string();
    let mut parts = Vec::new();
    for (i, f) in fields.iter().enumerate() {
        if skipped(f)? {
            continue;
        }
        let ty = &f.ty;
        let fname = f.ident.as_ref().map_or(i.to_string(), |n| n.to_string());
        text += &format!(" {fname}:{}", quote!(#ty));
        parts.push(quote!(<#ty as ::snapshot::Snap>::SCHEMA));
    }
    Ok(quote!(::snapshot::mix(::snapshot::fnv(#text.as_bytes()), &[#(#parts),*])))
}

fn expand(input: &DeriveInput) -> syn::Result<TokenStream> {
    let name = &input.ident;
    let (imp, ty, wh) = input.generics.split_for_impl();
    let (schema, save, load) = match &input.data {
        Data::Struct(s) => {
            let mut save = Vec::new();
            let mut load = Vec::new();
            for (i, f) in s.fields.iter().enumerate() {
                if skipped(f)? {
                    continue;
                }
                let member = match &f.ident {
                    Some(n) => quote!(#n),
                    None => {
                        let idx = syn::Index::from(i);
                        quote!(#idx)
                    }
                };
                save.push(quote!(::snapshot::Snap::save(&self.#member, w);));
                load.push(quote!(::snapshot::Snap::load(&mut self.#member, r)?;));
            }
            (schema(&name.to_string(), &s.fields)?, quote!(#(#save)*), quote!(#(#load)* Ok(())))
        }
        Data::Enum(e) => {
            let mut schemas = Vec::new();
            let mut save_arms = Vec::new();
            let mut load_arms = Vec::new();
            for (tag, v) in e.variants.iter().enumerate() {
                let tag = tag as u32;
                let vname = &v.ident;
                schemas.push(schema(&vname.to_string(), &v.fields)?);
                let binds: Vec<Ident> = (0..v.fields.len()).map(|i| format_ident!("f{i}")).collect();
                let mut saves = Vec::new();
                let mut news = Vec::new();
                for (f, b) in v.fields.iter().zip(&binds) {
                    if skipped(f)? {
                        news.push(quote!(::core::default::Default::default()));
                    } else {
                        saves.push(quote!(::snapshot::Snap::save(#b, w);));
                        news.push(quote!(::snapshot::read_new(r)?));
                    }
                }
                let (pat, build) = match &v.fields {
                    Fields::Named(n) => {
                        let names: Vec<_> = n.named.iter().map(|f| f.ident.clone().unwrap()).collect();
                        (quote!(Self::#vname { #(#names: #binds),* }), quote!(Self::#vname { #(#names: #news),* }))
                    }
                    Fields::Unnamed(_) => (quote!(Self::#vname(#(#binds),*)), quote!(Self::#vname(#(#news),*))),
                    Fields::Unit => (quote!(Self::#vname), quote!(Self::#vname)),
                };
                save_arms.push(quote!(#[allow(unused_variables)] #pat => { w.u32(#tag); #(#saves)* }));
                load_arms.push(quote!(#tag => #build,));
            }
            let text = name.to_string();
            let schema = quote!(::snapshot::mix(::snapshot::fnv(#text.as_bytes()), &[#(#schemas),*]));
            let save = quote!(match self { #(#save_arms)* });
            let load = quote! {
                *self = match r.u32()? {
                    #(#load_arms)*
                    t => return Err(::snapshot::Error::new(format!("bad {} variant {t}", #text))),
                };
                Ok(())
            };
            (schema, save, load)
        }
        Data::Union(_) => return Err(syn::Error::new(name.span(), "Snap cannot be derived for unions")),
    };
    Ok(quote! {
        impl #imp ::snapshot::Snap for #name #ty #wh {
            const SCHEMA: u64 = #schema;
            #[allow(unused_variables)]
            fn save(&self, w: &mut ::snapshot::Writer) { #save }
            #[allow(unused_variables)]
            fn load(&mut self, r: &mut ::snapshot::Reader<'_>) -> ::snapshot::Result<()> { #load }
        }
    })
}
