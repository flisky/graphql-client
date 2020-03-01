mod enums;
mod selection;

use crate::{
    field_type::GraphqlTypeQualifier,
    normalization::Normalization,
    resolution::*,
    shared::{field_rename_annotation, keyword_replace},
    GraphQLClientCodegenOptions,
};
use heck::SnakeCase;
use proc_macro2::{Ident, Span, TokenStream};
use quote::quote;
use selection::*;

/// The main code generation function.
pub(crate) fn response_for_query(
    operation: OperationRef<'_>,
    options: &GraphQLClientCodegenOptions,
) -> anyhow::Result<TokenStream> {
    let all_used_types = operation.all_used_types();
    let response_derives = render_derives(options.all_response_derives());

    let scalar_definitions = generate_scalar_definitions(&operation, &all_used_types);
    let enum_definitions = enums::generate_enum_definitions(&operation, &all_used_types, options);
    let fragment_definitions =
        generate_fragment_definitions(&operation, &all_used_types, &response_derives, options);
    let input_object_definitions =
        generate_input_object_definitions(&operation, &all_used_types, options);
    let variables_struct = generate_variables_struct(&operation, options);

    let definitions = render_response_data_fields(&operation, &response_derives, options);

    let q = quote! {
        use serde::{Serialize, Deserialize};

        #[allow(dead_code)]
        type Boolean = bool;
        #[allow(dead_code)]
        type Float = f64;
        #[allow(dead_code)]
        type Int = i64;
        #[allow(dead_code)]
        type ID = String;

        #(#scalar_definitions)*

        #(#enum_definitions)*

        #(#input_object_definitions)*

        #variables_struct

        #(#fragment_definitions)*

        #definitions
    };

    // panic!("{}", q);

    Ok(q)
}

fn generate_variables_struct(
    operation: &OperationRef<'_>,
    options: &GraphQLClientCodegenOptions,
) -> TokenStream {
    let variable_derives = options.all_variable_derives();
    let variable_derives = render_derives(variable_derives);

    if operation.has_no_variables() {
        return quote!(
            #variable_derives
            pub struct Variables;
        );
    }

    let variable_fields = operation.variables().map(generate_variable_struct_field);
    let variable_defaults = operation.variables().map(|variable| {
        let method_name = format!("default_{}", variable.name());
        let method_name = Ident::new(&method_name, Span::call_site());
        let method_return_type = render_variable_field_type(variable);

        quote!(
            pub fn #method_name() -> #method_return_type {
                todo!()
            }
        )
    });

    let variables_struct = quote!(
        #variable_derives
        pub struct Variables {
            #(#variable_fields,)*
        }

        impl Variables {
            #(#variable_defaults)*
        }
    );

    variables_struct.into()
}

fn generate_variable_struct_field(variable: VariableRef<'_>) -> TokenStream {
    let snake_case_name = variable.name().to_snake_case();
    let ident = Ident::new(
        &crate::shared::keyword_replace(&snake_case_name),
        Span::call_site(),
    );
    let annotation = crate::shared::field_rename_annotation(variable.name(), &snake_case_name);
    let r#type = render_variable_field_type(variable);

    quote::quote!(#annotation pub #ident : #r#type)
}

fn generate_scalar_definitions<'a, 'schema: 'a>(
    operation: &OperationRef<'schema>,
    all_used_types: &'a crate::resolution::UsedTypes,
) -> impl Iterator<Item = TokenStream> + 'a {
    all_used_types.scalars(operation.schema()).map(|scalar| {
        let ident = syn::Ident::new(scalar.name(), proc_macro2::Span::call_site());
        quote!(type #ident = super::#ident;)
    })
}

fn render_derives<'a>(derives: impl Iterator<Item = &'a str>) -> impl quote::ToTokens {
    let idents = derives.map(|s| Ident::new(s, Span::call_site()));

    quote!(#[derive(#(#idents),*)])
}

fn render_variable_field_type(variable: VariableRef<'_>) -> TokenStream {
    let full_name = Ident::new(variable.type_name(), Span::call_site());

    decorate_type(&full_name, variable.type_qualifiers())
}

fn decorate_type(ident: &Ident, qualifiers: &[GraphqlTypeQualifier]) -> TokenStream {
    let mut qualified = quote!(#ident);

    let mut non_null = false;

    // Note: we iterate over qualifiers in reverse because it is more intuitive. This
    // means we start from the _inner_ type and make our way to the outside.
    for qualifier in qualifiers.iter().rev() {
        match (non_null, qualifier) {
            // We are in non-null context, and we wrap the non-null type into a list.
            // We switch back to null context.
            (true, GraphqlTypeQualifier::List) => {
                qualified = quote!(Vec<#qualified>);
                non_null = false;
            }
            // We are in nullable context, and we wrap the nullable type into a list.
            (false, GraphqlTypeQualifier::List) => {
                qualified = quote!(Vec<Option<#qualified>>);
            }
            // We are in non-nullable context, but we can't double require a type
            // (!!).
            (true, GraphqlTypeQualifier::Required) => panic!("double required annotation"),
            // We are in nullable context, and we switch to non-nullable context.
            (false, GraphqlTypeQualifier::Required) => {
                non_null = true;
            }
        }
    }

    // If we are in nullable context at the end of the iteration, we wrap the whole
    // type with an Option.
    if !non_null {
        qualified = quote!(Option<#qualified>);
    }

    qualified
}

fn generate_input_object_definitions(
    operation: &OperationRef<'_>,
    all_used_types: &UsedTypes,
    options: &GraphQLClientCodegenOptions,
) -> Vec<TokenStream> {
    all_used_types
        .inputs(operation.schema())
        .map(|input| {
            let struct_name = Ident::new(input.name(), Span::call_site());
            quote!(pub struct #struct_name;)
        })
        .collect()
}

fn generate_fragment_definitions(
    operation: &OperationRef<'_>,
    all_used_types: &UsedTypes,
    response_derives: &impl quote::ToTokens,
    options: &GraphQLClientCodegenOptions,
) -> Vec<TokenStream> {
    let mut fragment_definitions = Vec::with_capacity(all_used_types.fragments_len());

    let fragments = all_used_types
        .fragment_ids()
        .map(move |id| operation.query().get_fragment_ref(operation.schema(), id));

    for fragment in fragments {
        fragment_definitions.push(selection::render_fragment(
            &fragment,
            response_derives,
            options,
        ));
    }

    fragment_definitions
}

/// Render a struct for a selection on an object or interface.
fn render_object_like_struct(
    response_derives: &impl quote::ToTokens,
    struct_name: &str,
    fields: &[TokenStream],
    variants: &[TokenStream],
) -> TokenStream {
    let (on_field, on_enum) = if variants.len() > 0 {
        let enum_name_str = format!("{}On", struct_name);
        let enum_name = Ident::new(&enum_name_str, Span::call_site());

        (
            Some(quote!(#[serde(flatten)] pub on: #enum_name,)),
            Some(render_union_enum(
                response_derives,
                &enum_name_str,
                variants,
            )),
        )
    } else {
        (None, None)
    };

    let struct_ident = Ident::new(struct_name, Span::call_site());

    quote! {
        #response_derives
        pub struct #struct_ident {
            #(#fields,)*
            #on_field
        }

        #on_enum
    }
}

fn render_union_enum(
    response_derives: &impl quote::ToTokens,
    enum_name: &str,
    variants: &[TokenStream],
) -> TokenStream {
    let enum_ident = Ident::new(enum_name, Span::call_site());

    quote! {
        #response_derives
        #[serde(tag = "__typename")]
        pub enum #enum_ident {
            #(#variants,)*
        }
    }
}
