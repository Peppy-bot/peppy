use proc_macro2::{Ident, TokenStream};

#[derive(Clone, Debug)]
pub struct FunctionParam {
    pub ident: Ident,
    pub ty: TokenStream,
}

impl FunctionParam {
    pub fn new(ident: Ident, ty: TokenStream) -> Self {
        Self { ident, ty }
    }

    pub fn ident(&self) -> &Ident {
        &self.ident
    }

    pub fn ty(&self) -> &TokenStream {
        &self.ty
    }
}
