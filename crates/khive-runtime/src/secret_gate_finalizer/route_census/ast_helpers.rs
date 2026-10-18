use proc_macro2::{TokenStream, TokenTree};
use syn::{Attribute, Expr, ExprLit, Lit};

pub(super) fn attribute_name(attr: &Attribute) -> String {
    attr.path()
        .segments
        .iter()
        .map(|part| part.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

pub(super) fn test_only(attrs: &[Attribute]) -> bool {
    fn requires_test(meta: &syn::Meta) -> bool {
        match meta {
            syn::Meta::Path(path) => path.is_ident("test"),
            syn::Meta::List(list) if list.path.is_ident("all") => list
                .parse_args_with(
                    syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                )
                .is_ok_and(|items| items.iter().any(requires_test)),
            syn::Meta::List(list) if list.path.is_ident("any") => list
                .parse_args_with(
                    syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                )
                .is_ok_and(|items| !items.is_empty() && items.iter().all(requires_test)),
            _ => false,
        }
    }

    attrs.iter().any(|attr| {
        let name = attribute_name(attr);
        if name == "test" || name == "tokio::test" {
            return true;
        }
        name == "cfg"
            && matches!(&attr.meta, syn::Meta::List(list)
                if list.parse_args::<syn::Meta>().is_ok_and(|meta| requires_test(&meta)))
    })
}

pub(super) fn literal_string(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Lit(ExprLit {
            lit: Lit::Str(value),
            ..
        }) => Some(value.value()),
        Expr::Reference(reference) => literal_string(&reference.expr),
        Expr::Paren(paren) => literal_string(&paren.expr),
        _ => None,
    }
}

pub(super) fn macro_strings(tokens: TokenStream, strings: &mut Vec<String>) {
    for token in tokens {
        match token {
            TokenTree::Literal(literal) => {
                if let Ok(value) = syn::parse_str::<syn::LitStr>(&literal.to_string()) {
                    strings.push(value.value());
                }
            }
            TokenTree::Group(group) => macro_strings(group.stream(), strings),
            _ => {}
        }
    }
}
