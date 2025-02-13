mod ctx;
mod search_table;

use ctx::CgCtx;

use super::simplify::Trans;
use super::{State, StateIdx, DFA};

use crate::ast::{RuleKind, RuleRhs};
use crate::collections::{Map, Set};
use crate::nfa::AcceptingState;
use crate::range_map::{Range, RangeMap};
use crate::right_ctx::{RightCtxDFAs, RightCtxIdx};
use crate::semantic_action_table::{SemanticActionIdx, SemanticActionTable};

use std::convert::TryFrom;

use proc_macro2::{Span, TokenStream};
use quote::{quote, ToTokens};
use syn::fold::Fold;
use syn::visit::Visit;

// Max. size for guards in ranges. When a case have more ranges than this we generate a binary
// search table.
//
// Using binary search for large number of guards should be more efficient in runtime, but more
// importantly, when using builtin regexes like `$$uppercase` that has a lot of cases (see
// `char_ranges` module), rustc uses GiBs of RAM when compiling the generated code, even in debug
// mode. For example, the test `builtins` takes more than 32GiB of memory to compile.
//
// Binary search does less comparisons in the worst case when we have more than 3 cases, but the
// code for binary search is more complicated than a chain of `||`s, so I think it makes sense to
// have a slightly larger number here.
const MAX_GUARD_SIZE: usize = 9;

/// A [`Visit`] implementation to collect user state lifetimes.
///
/// Lifetime `'input` is ignored as `'input` is added to the generated lexer struct regardless of
/// whether the user state uses it or not.
///
/// Lifetime `'static` is ignored as `'static` cannot be added as a lifetime parameter.
struct LifetimeVisitor<'a> {
    lifetimes: &'a mut Vec<syn::Lifetime>,
}

impl<'ast, 'a> Visit<'ast> for LifetimeVisitor<'a> {
    fn visit_lifetime(&mut self, node: &'ast syn::Lifetime) {
        if node.ident != "static" && node.ident != "input" {
            self.lifetimes.push(node.clone())
        }
    }
}

/// A [`Fold`] implementation for replacing `'input` lifetimes in a [`syn`] type with `'static`.
///
/// This is used to replace `'input`s in user state lifetimes with `'static`s in `impl` blocks for
/// initializing the lexer without an input string.
struct SubstLifetimeFolder;

impl Fold for SubstLifetimeFolder {
    fn fold_lifetime(&mut self, node: syn::Lifetime) -> syn::Lifetime {
        if node.ident == "input" {
            syn::Lifetime::new("'static", node.span())
        } else {
            node
        }
    }
}

pub fn reify(
    dfa: DFA<Trans<SemanticActionIdx>, SemanticActionIdx>,
    right_ctx_dfas: &RightCtxDFAs<StateIdx>,
    semantic_actions: SemanticActionTable,
    user_state_type: Option<syn::Type>,
    user_error_type: Option<syn::Type>,
    rule_states: Map<String, StateIdx>,
    lexer_name: syn::Ident,
    token_type: syn::Type,
    public: bool,
) -> TokenStream {
    let rule_name_enum_name =
        syn::Ident::new(&(lexer_name.to_string() + "Rule"), lexer_name.span());

    let rule_name_idents: Vec<syn::Ident> = rule_states
        .keys()
        .map(|rule_name| syn::Ident::new(rule_name, Span::call_site()))
        .collect();

    let visibility = if public { quote!(pub) } else { quote!() };

    let mut ctx = CgCtx::new(
        &dfa,
        semantic_actions,
        lexer_name,
        token_type,
        user_error_type,
        rule_states,
    );

    let (user_state_lifetimes, user_state_type, user_state_type_static) = match user_state_type {
        None => (Vec::new(), quote!(()), syn::Type::Verbatim(quote!(()))),
        Some(ty) => {
            let mut lifetimes = Vec::new();
            {
                let mut visitor = LifetimeVisitor {
                    lifetimes: &mut lifetimes,
                };
                visitor.visit_type(&ty);
            }
            let subst = SubstLifetimeFolder.fold_type(ty.clone());
            (lifetimes, ty.into_token_stream(), subst)
        }
    };

    let match_arms = generate_state_arms(&mut ctx, dfa);

    let switch_method = generate_switch(&ctx, &rule_name_enum_name);

    let token_type = ctx.token_type();

    let error_type = match ctx.user_error_type() {
        None => quote!(::std::convert::Infallible),
        Some(error_type) => error_type.into_token_stream(),
    };

    let semantic_action_fn_ret_ty = match ctx.user_error_type() {
        None => {
            quote!(::lexgen_util::SemanticActionResult<Result<#token_type, ::std::convert::Infallible>>)
        }
        Some(user_error_type) => {
            quote!(::lexgen_util::SemanticActionResult<Result<#token_type, #user_error_type>>)
        }
    };

    let semantic_action_fns =
        generate_semantic_action_fns(&ctx, &user_state_lifetimes, &semantic_action_fn_ret_ty);

    let right_ctx_fns = generate_right_ctx_fns(&mut ctx, right_ctx_dfas);

    let search_tables = ctx.take_search_tables();

    let binary_search_fn = if search_tables.is_empty() {
        quote!()
    } else {
        quote!(
            fn binary_search(c: char, table: &[(char, char)]) -> bool {
                table
                    .binary_search_by(|(start, end)| match c.cmp(start) {
                        std::cmp::Ordering::Greater => {
                            if c <= *end {
                                std::cmp::Ordering::Equal
                            } else {
                                std::cmp::Ordering::Less
                            }
                        }
                        std::cmp::Ordering::Equal => std::cmp::Ordering::Equal,
                        std::cmp::Ordering::Less => std::cmp::Ordering::Greater,
                    })
                    .is_ok()
            }
        )
    };

    let search_tables: Vec<TokenStream> = search_tables
        .iter()
        .map(|(ranges, ident)| {
            let n_ranges = ranges.len();
            let pairs: Vec<TokenStream> = ranges
                .iter()
                .map(|(start, end)| quote!((#start, #end)))
                .collect();
            quote!(
                static #ident: [(char, char); #n_ranges] = [
                    #(#pairs),*
                ];
            )
        })
        .collect();

    let token_type = ctx.token_type();

    // Name of the type synonym:
    // type #lexer_name<'input, I> = #lexer_name_<'input, I, #user_state_type>
    let lexer_name = ctx.lexer_name();

    // Name of the lexer struct:
    // struct #lexer_struct_name<'input, I: Iterator<Item = char> + Clone, S> { ... }
    //
    // This struct with a state parameter is needed to be able to have `impl` blocks that require
    // `S: Default` in the methods without requiring the user state to implement `Default` when
    // those methods are not used.
    let lexer_struct_name = syn::Ident::new(&(lexer_name.to_string() + "_"), lexer_name.span());

    quote!(
        // An enum for the rule sets in the DFA. `Init` is the initial, unnamed rule set.
        #[derive(Clone, Copy)]
        enum #rule_name_enum_name {
            #(#rule_name_idents,)*
        }

        #visibility struct #lexer_struct_name<'input, #(#user_state_lifetimes,)* I: Iterator<Item = char> + Clone, S>(
            ::lexgen_util::Lexer<
                'input,
                I,
                #token_type,
                S,
                #error_type,
                #lexer_name<'input, #(#user_state_lifetimes,)* I>
            >
        );

        #visibility type #lexer_name<'input, #(#user_state_lifetimes,)* I> =
            #lexer_struct_name<'input, #(#user_state_lifetimes,)* I, #user_state_type>;

        // Methods below for using in semantic actions
        impl<'input, #(#user_state_lifetimes,)* I: Iterator<Item = char> + Clone, S>
                #lexer_struct_name<'input, #(#user_state_lifetimes,)* I, S>
        {
            fn switch_and_return<T>(&mut self, rule: #rule_name_enum_name, token: T) -> ::lexgen_util::SemanticActionResult<T> {
                self.switch::<T>(rule);
                ::lexgen_util::SemanticActionResult::Return(token)
            }

            fn return_<T>(&self, token: T) -> ::lexgen_util::SemanticActionResult<T> {
                ::lexgen_util::SemanticActionResult::Return(token)
            }

            #switch_method

            fn continue_<T>(&self) -> ::lexgen_util::SemanticActionResult<T> {
                ::lexgen_util::SemanticActionResult::Continue
            }

            fn state(&mut self) -> &mut S {
                self.0.state()
            }

            fn reset_match(&mut self) {
                self.0.reset_match()
            }

            fn match_(&self) -> &'input str {
                self.0.match_()
            }

            fn match_loc(&self) -> (::lexgen_util::Loc, ::lexgen_util::Loc) {
                self.0.match_loc()
            }

            fn peek(&mut self) -> Option<char> {
                self.0.peek()
            }
        }

        impl<'input, #(#user_state_lifetimes,)* S: ::std::default::Default>
                #lexer_struct_name<'input, #(#user_state_lifetimes,)* ::std::str::Chars<'input>, S>
        {
            #visibility fn new(input: &'input str) -> Self {
                #lexer_struct_name(::lexgen_util::Lexer::new(input))
            }
        }

        impl<'input #(,#user_state_lifetimes)*>
                #lexer_struct_name<'input, #(#user_state_lifetimes,)* ::std::str::Chars<'input>, #user_state_type>
        {
            #visibility fn new_with_state(input: &'input str, user_state: #user_state_type) -> Self {
                #lexer_struct_name(::lexgen_util::Lexer::new_with_state(input, user_state))
            }
        }

        impl<#(#user_state_lifetimes,)* I: Iterator<Item = char> + Clone, S: ::std::default::Default>
                #lexer_struct_name<'static, #(#user_state_lifetimes,)* I, S>
        {
            #visibility fn new_from_iter(iter: I) -> Self {
                #lexer_struct_name(::lexgen_util::Lexer::new_from_iter(iter))
            }
        }

        impl<#(#user_state_lifetimes,)* I: Iterator<Item = char> + Clone>
                #lexer_struct_name<'static, #(#user_state_lifetimes,)* I, #user_state_type_static>
        {
            #visibility fn new_from_iter_with_state(iter: I, user_state: #user_state_type_static) -> Self {
                #lexer_struct_name(::lexgen_util::Lexer::new_from_iter_with_state(iter, user_state))
            }
        }

        #(#search_tables)*
        #binary_search_fn
        #semantic_action_fns
        #(#right_ctx_fns)*

        impl<'input, #(#user_state_lifetimes,)* I: Iterator<Item = char> + Clone> Iterator for
                #lexer_struct_name<'input, #(#user_state_lifetimes,)* I, #user_state_type>
        {
            type Item = Result<(::lexgen_util::Loc, #token_type, ::lexgen_util::Loc), ::lexgen_util::LexerError<#error_type>>;

            fn next(&mut self) -> Option<Self::Item> {
                loop {
                    if self.0.__done {
                        return None;
                    }

                    // println!("state = {:?}, next char = {:?}", self.0.__state, self.0.peek());
                    match self.0.__state {
                        #(#match_arms,)*
                    }
                }
            }
        }
    )
}

fn generate_switch(ctx: &CgCtx, enum_name: &syn::Ident) -> TokenStream {
    let mut arms: Vec<TokenStream> = vec![];

    for (rule_name, state_idx) in ctx.rule_states().iter() {
        let StateIdx(state_idx) = ctx.renumber_state(*state_idx);
        let rule_ident = syn::Ident::new(rule_name, Span::call_site());
        arms.push(quote!(
            #enum_name::#rule_ident =>
                self.0.__state = #state_idx
        ));
    }

    quote!(
        fn switch<A>(&mut self, rule: #enum_name) -> ::lexgen_util::SemanticActionResult<A> {
            match rule {
                #(#arms,)*
            }
            self.0.__initial_state = self.0.__state;
            ::lexgen_util::SemanticActionResult::Continue
        }
    )
}

/// Generate arms of `match self.__state { ... }` of a DFA.
fn generate_state_arms(
    ctx: &mut CgCtx,
    dfa: DFA<Trans<SemanticActionIdx>, SemanticActionIdx>,
) -> Vec<TokenStream> {
    let DFA { states } = dfa;

    let mut match_arms: Vec<TokenStream> = vec![];

    let n_states = states.len();

    for (state_idx, state) in states.iter().enumerate() {
        if state.predecessors.len() == 1 && !state.initial {
            continue;
        }

        let state_code: TokenStream = generate_state_arm(ctx, state_idx, state, &states);

        let StateIdx(state_idx) = ctx.renumber_state(StateIdx(state_idx));
        let state_idx_pat = if state_idx == n_states - ctx.n_inlined_states() - 1 {
            quote!(_)
        } else {
            quote!(#state_idx)
        };

        match_arms.push(quote!(
            #state_idx_pat => { #state_code }
        ));
    }

    match_arms
}

// NB. Does not generate braces around the code
fn generate_state_arm(
    ctx: &mut CgCtx,
    state_idx: usize,
    state: &State<Trans<SemanticActionIdx>, SemanticActionIdx>,
    states: &[State<Trans<SemanticActionIdx>, SemanticActionIdx>],
) -> TokenStream {
    let State {
        initial,
        char_transitions,
        range_transitions,
        any_transition,
        end_of_input_transition,
        accepting,
        predecessors: _,
    } = state;

    let fail = || -> TokenStream {
        let action = generate_semantic_action_call(&quote!(semantic_action));
        quote!(match self.0.backtrack() {
            Err(err) => return Some(Err(err)),
            Ok(semantic_action) => #action,
        })
    };

    // When we can't take char or range transitions, take the 'any' transition if it exists, or
    // fail (backtrack or raise error)
    let default_action = any_transition
        .as_ref()
        .map(|any_transition| generate_any_transition(ctx, states, any_transition, fail()))
        .unwrap_or_else(fail);

    let state_char_arms = generate_state_char_arms(
        ctx,
        states,
        char_transitions,
        range_transitions,
        &default_action,
    );

    // In initial state (rule `Init`) unhandled end-of-input yields `None`. In other states we
    // expect to see a end-of-input handler, or fail with "unexpected end-of-input".
    let end_of_input_default_action = if state_idx == 0 {
        quote!(return None;)
    } else {
        fail()
    };

    let end_of_input_action = match end_of_input_transition {
        Some(end_of_input_transition) => match end_of_input_transition {
            Trans::Accept(accepting_states) => {
                test_right_ctxs(ctx, accepting_states, end_of_input_default_action)
            }
            Trans::Trans(next_state) => {
                let StateIdx(next_state) = ctx.renumber_state(*next_state);
                quote!(self.0.__state = #next_state;)
            }
        },
        None => end_of_input_default_action,
    };

    let end_of_input_action = quote!(
        self.0.__done = true; // don't handle end-of-input again
        #end_of_input_action
    );

    if state_idx == 0 {
        assert!(initial);

        // See #12 for the special case in state 0 (rule Init)
        quote!(
            self.reset_match();

            match self.0.next() {
                None => {
                    #end_of_input_action
                }
                Some(char) => {
                    match char {
                        #(#state_char_arms,)*
                    }
                }
            }
        )
    } else if !accepting.is_empty() {
        // Accepting state
        let mut rhss: Vec<(TokenStream, TokenStream)> = Vec::with_capacity(accepting.len());
        let mut default = quote!();

        for AcceptingState { value, right_ctx } in accepting.iter() {
            match right_ctx {
                Some(right_ctx) => {
                    let right_ctx_fn = right_ctx_fn_name(ctx.lexer_name(), right_ctx);
                    let semantic_fn = ctx.semantic_action_fn_ident(*value);
                    rhss.push((
                        quote!(#right_ctx_fn(self.0.__iter.clone())),
                        quote!(self.0.set_accepting_state(#semantic_fn)),
                    ));
                }
                None => {
                    let semantic_fn = ctx.semantic_action_fn_ident(*value);
                    default = quote!(self.0.set_accepting_state(#semantic_fn););
                    break;
                }
            }
        }

        let mut set_accepting_state = default;

        for (cond, rhs) in rhss.into_iter().rev() {
            set_accepting_state = quote!(if #cond { #rhs } else { #set_accepting_state });
        }

        quote!(
            #set_accepting_state

            match self.0.next() {
                None => {
                    #end_of_input_action
                }
                Some(char) => {
                    match char {
                        #(#state_char_arms,)*
                    }
                }
            }
        )
    } else {
        // Non-accepting state
        quote!(match self.0.next() {
            None => {
                #end_of_input_action
            }
            Some(char) => {
                match char {
                    #(#state_char_arms,)*
                }
            }
        })
    }
}

fn generate_any_transition(
    ctx: &mut CgCtx,
    states: &[State<Trans<SemanticActionIdx>, SemanticActionIdx>],
    trans: &Trans<SemanticActionIdx>,
    fail: TokenStream,
) -> TokenStream {
    let action = match trans {
        Trans::Trans(StateIdx(next_state)) => {
            if states[*next_state].predecessors.len() == 1 {
                generate_state_arm(ctx, *next_state, &states[*next_state], states)
            } else {
                let StateIdx(next_state) = ctx.renumber_state(StateIdx(*next_state));
                quote!(self.0.__state = #next_state;)
            }
        }

        Trans::Accept(accepting_states) => test_right_ctxs(ctx, accepting_states, fail),
    };

    quote!(
        #action
    )
}

/// Generate arms for `match char { ... }`
fn generate_state_char_arms(
    ctx: &mut CgCtx,
    states: &[State<Trans<SemanticActionIdx>, SemanticActionIdx>],
    char_transitions: &Map<char, Trans<SemanticActionIdx>>,
    range_transitions: &RangeMap<Trans<SemanticActionIdx>>,
    // RHS of the default alternative for this `match` (_ => <default_rhs>)
    default_rhs: &TokenStream,
) -> Vec<TokenStream> {
    // Arms of the `match` for the current character
    let mut state_char_arms: Vec<TokenStream> = vec![];

    // Collect characters for next states, to be able to use or patterns in arms and reduce code
    // size
    let mut state_chars: Map<StateIdx, Vec<char>> = Default::default();
    for (char, next) in char_transitions {
        match next {
            Trans::Accept(accepting) => {
                let action_code = test_right_ctxs(ctx, accepting, default_rhs.clone());
                state_char_arms.push(quote!(
                    #char => {
                        #action_code
                    }
                ));
            }
            Trans::Trans(state_idx) => state_chars.entry(*state_idx).or_default().push(*char),
        }
    }

    // Add char transitions
    for (StateIdx(next_state), chars) in state_chars.iter() {
        let pat = quote!(#(#chars)|*);

        let next = if states[*next_state].predecessors.len() == 1 {
            generate_state_arm(ctx, *next_state, &states[*next_state], states)
        } else {
            let StateIdx(next_state) = ctx.renumber_state(StateIdx(*next_state));
            quote!(
                self.0.__state = #next_state;
            )
        };

        state_char_arms.push(quote!(
            #pat => {
                #next
            }
        ));
    }

    // Same as above for range transitions. Use chain of "or"s for ranges with same transition.
    let mut state_ranges: Map<StateIdx, Vec<(char, char)>> = Default::default();

    for range in range_transitions.iter() {
        match &range.value {
            Trans::Trans(state_idx) => state_ranges.entry(*state_idx).or_default().push((
                char::try_from(range.start).unwrap(),
                char::try_from(range.end).unwrap(),
            )),
            Trans::Accept(accepting) => {
                let action_code = test_right_ctxs(ctx, accepting, default_rhs.clone());

                let range_start = char::from_u32(range.start).unwrap();
                let range_end = char::from_u32(range.end).unwrap();

                let range_check = inclusive_range_contains(quote!(x), range_start, range_end);
                state_char_arms.push(quote!(
                    x if #range_check => {
                        #action_code
                    }
                ));
            }
        }
    }

    // Add range transitions
    for (StateIdx(next_state), ranges) in state_ranges.into_iter() {
        let guard = if ranges.len() > MAX_GUARD_SIZE {
            let binary_search_table_id = ctx.add_search_table(ranges);

            quote!(binary_search(x, &#binary_search_table_id))
        } else {
            let range_checks: Vec<TokenStream> = ranges
                .into_iter()
                .map(|(range_begin, range_end)| {
                    if range_begin == range_end {
                        quote!(x == #range_begin)
                    } else {
                        inclusive_range_contains(quote!(x), range_begin, range_end)
                    }
                })
                .collect();

            quote!(#(#range_checks)||*)
        };

        let next = if states[next_state].predecessors.len() == 1 {
            generate_state_arm(ctx, next_state, &states[next_state], states)
        } else {
            let StateIdx(next_state) = ctx.renumber_state(StateIdx(next_state));
            quote!(
                self.0.__state = #next_state;
            )
        };

        state_char_arms.push(quote!(
            x if #guard => {
                #next
            }
        ));
    }

    state_char_arms.push(quote!(_ => { #default_rhs }));

    state_char_arms
}

/// Generate call to the semantic action function with the given index and handle the result.
fn generate_rhs_code(ctx: &CgCtx, action: SemanticActionIdx) -> TokenStream {
    let semantic_action_call =
        generate_semantic_action_call(&ctx.semantic_action_fn_ident(action).into_token_stream());

    quote!(
        self.0.reset_accepting_state();
        #semantic_action_call
    )
}

/// Generate call to the given semantic action function and handle the result.
fn generate_semantic_action_call(action_fn: &TokenStream) -> TokenStream {
    let map_res = quote!(match res {
        Ok(tok) => Ok((match_start, tok, match_end)),
        Err(err) => Err(::lexgen_util::LexerError {
            location: self.match_loc().0,
            kind: ::lexgen_util::LexerErrorKind::Custom(err),
        }),
    });

    quote!(match #action_fn(self) {
        ::lexgen_util::SemanticActionResult::Continue => {
            self.0.__state = self.0.__initial_state;
        }
        ::lexgen_util::SemanticActionResult::Return(res) => {
            self.0.__state = self.0.__initial_state;
            let (match_start, match_end) = self.match_loc();
            self.0.reset_match();
            return Some(#map_res);
        }
    })
}

fn generate_semantic_action_fns(
    ctx: &CgCtx,
    user_state_lifetimes: &Vec<syn::Lifetime>,
    semantic_action_fn_ret_ty: &TokenStream,
) -> TokenStream {
    let lexer_name = ctx.lexer_name();
    let token_type = ctx.token_type();

    let fns: Vec<TokenStream> = ctx
        .iter_semantic_actions()
        .map(|(idx, action)| {
            let ident = ctx.semantic_action_fn_ident(idx);

            let rhs = match action {
                RuleRhs::None => {
                    quote!(|__lexer: &mut #lexer_name<'input, #(#user_state_lifetimes, )* I>| __lexer.continue_().map_token(Ok))
                }

                RuleRhs::Rhs { expr, kind } => {
                    match kind {
                        RuleKind::Simple => {
                            quote!(|__lexer: &'lexer mut #lexer_name<'input, #(#user_state_lifetimes, )* I>| __lexer.return_(#expr).map_token(Ok))
                        }
                        RuleKind::Fallible => quote!(#expr),
                        RuleKind::Infallible => {
                            quote!(|__lexer: &'lexer mut #lexer_name<'input, #(#user_state_lifetimes, )* I>| {
                                let semantic_action:
                                    fn(&'lexer mut #lexer_name<'input, #(#user_state_lifetimes, )* I>) -> ::lexgen_util::SemanticActionResult<#token_type> =
                                        #expr;

                                semantic_action(__lexer).map_token(Ok)
                            })
                        }
                    }
                }
            };

            quote!(
                #[allow(non_snake_case)]
                fn #ident<'lexer, #(#user_state_lifetimes, )* 'input, I: Iterator<Item = char> + Clone>(lexer: &'lexer mut #lexer_name<'input, #(#user_state_lifetimes, )* I>) -> #semantic_action_fn_ret_ty {
                    let action: fn(&'lexer mut #lexer_name<'input, #(#user_state_lifetimes, )* I>) -> #semantic_action_fn_ret_ty = #rhs;
                    action(lexer)
                }
            )
        })
        .collect();

    quote!(#(#fns)*)
}

fn right_ctx_fn_name(lexer_name: &syn::Ident, idx: &RightCtxIdx) -> syn::Ident {
    syn::Ident::new(
        &format!("{}_RIGHT_CTX_{}", lexer_name, idx.as_usize()),
        Span::call_site(),
    )
}

fn generate_right_ctx_fns(
    ctx: &mut CgCtx,
    right_ctx_dfas: &RightCtxDFAs<StateIdx>,
) -> Vec<TokenStream> {
    let mut fns = vec![];

    let lexer_name = ctx.lexer_name().clone();

    for (idx, dfa) in right_ctx_dfas.iter() {
        let fn_name = right_ctx_fn_name(&lexer_name, &idx);

        let match_arms = generate_right_ctx_state_arms(ctx, dfa);

        fns.push(
            quote!(fn #fn_name<I: Iterator<Item = char> + Clone>(mut input: I) -> bool {
                let mut state: usize = 0;

                loop {
                    match state {
                        #(#match_arms)*
                    }
                }
            }),
        );
    }

    fns
}

fn generate_right_ctx_state_arms(ctx: &mut CgCtx, dfa: &DFA<StateIdx, ()>) -> Vec<TokenStream> {
    let DFA { states } = dfa;

    let mut match_arms: Vec<TokenStream> = vec![];

    let n_states = states.len();

    for (state_idx, state) in states.iter().enumerate() {
        let state_code: TokenStream = generate_right_ctx_state_arm(ctx, state, states);

        let state_idx_pat = if state_idx == n_states - 1 {
            quote!(_)
        } else {
            quote!(#state_idx)
        };

        match_arms.push(quote!(
            #state_idx_pat => { #state_code }
        ));
    }

    match_arms
}

fn generate_right_ctx_state_arm(
    ctx: &mut CgCtx,
    state: &State<StateIdx, ()>,
    states: &[State<StateIdx, ()>],
) -> TokenStream {
    let State {
        initial: _,
        char_transitions,
        range_transitions,
        any_transition,
        end_of_input_transition,
        accepting,
        predecessors: _,
    } = state;

    let state_char_arms =
        generate_right_ctx_state_char_arms(ctx, states, char_transitions, range_transitions);

    // Make sure right contexts don't have right contexts. We don't allow this in the syntax
    // currently.
    for accepting_state in accepting {
        assert_eq!(accepting_state.right_ctx, None);
    }

    if !accepting.is_empty() {
        return quote!(return true);
    }

    let eof = match end_of_input_transition {
        Some(StateIdx(eof_next)) => quote!(state = #eof_next),
        None => quote!(return false),
    };

    let def = match any_transition {
        Some(StateIdx(any_next)) => quote!(state = #any_next),
        None => quote!(return false),
    };

    quote!(
        match input.next() {
            None => #eof,
            Some(char) => {
                match char {
                    #(#state_char_arms,)*
                    _ => #def,
                }
            }
        }
    )
}

// NB. Does not add default case
fn generate_right_ctx_state_char_arms(
    ctx: &mut CgCtx,
    states: &[State<StateIdx, ()>],
    char_transitions: &Map<char, StateIdx>,
    range_transitions: &RangeMap<StateIdx>,
) -> Vec<TokenStream> {
    // Arms of the `match` for the current character
    let mut state_char_arms: Vec<TokenStream> = vec![];

    // Collect characters for next states, to be able to use or patterns in arms and reduce code
    // size
    let mut state_chars: Map<StateIdx, Vec<char>> = Default::default();

    // Set of chars that transition to an accepting state
    let mut accept_chars: Set<char> = Default::default();

    for (char, next) in char_transitions {
        if states[next.0].accepting.is_empty() {
            state_chars.entry(*next).or_default().push(*char);
        } else {
            accept_chars.insert(*char);
        }
    }

    // Add char transitions
    for (StateIdx(next_state), chars) in state_chars.iter() {
        let pat = quote!(#(#chars)|*);
        state_char_arms.push(quote!(#pat => self.state = #next_state));
    }

    if !accept_chars.is_empty() {
        let accept_chars: Vec<char> = accept_chars.into_iter().collect();
        state_char_arms.push(quote!(#(#accept_chars)|* => return true));
    }

    // Same as above for range transitions. Use chain of "or"s for ranges with same transition.
    let mut state_ranges: Map<StateIdx, Vec<(char, char)>> = Default::default();
    let mut accept_ranges: Set<(char, char)> = Default::default();

    for Range {
        start,
        end,
        value: next,
    } in range_transitions.iter()
    {
        let start = char::try_from(*start).unwrap();
        let end = char::try_from(*end).unwrap();

        if states[next.0].accepting.is_empty() {
            state_ranges.entry(*next).or_default().push((start, end));
        } else {
            accept_ranges.insert((start, end));
        }
    }

    // Add range transitions
    for (StateIdx(next_state), ranges) in state_ranges.into_iter() {
        let guard = if ranges.len() > MAX_GUARD_SIZE {
            let binary_search_table_id = ctx.add_search_table(ranges);

            quote!(binary_search(x, &#binary_search_table_id))
        } else {
            let range_checks: Vec<TokenStream> = ranges
                .into_iter()
                .map(|(range_begin, range_end)| {
                    inclusive_range_contains(quote!(x), range_begin, range_end)
                })
                .collect();

            quote!(#(#range_checks)||*)
        };

        state_char_arms.push(quote!(x if #guard => state = #next_state));
    }

    if !accept_ranges.is_empty() {
        let guard = if accept_ranges.len() > MAX_GUARD_SIZE {
            let binary_search_table_id = ctx.add_search_table(accept_ranges.into_iter().collect());

            quote!(binary_search(x, &#binary_search_table_id))
        } else {
            let range_checks: Vec<TokenStream> = accept_ranges
                .into_iter()
                .map(|(range_begin, range_end)| {
                    inclusive_range_contains(quote!(x), range_begin, range_end)
                })
                .collect();

            quote!(#(#range_checks)||*)
        };

        state_char_arms.push(quote!(x if #guard => return true));
    }

    state_char_arms
}

fn test_right_ctxs(
    ctx: &mut CgCtx,
    accepting_states: &[AcceptingState<SemanticActionIdx>],
    default_rhs: TokenStream,
) -> TokenStream {
    let mut alts: Vec<(TokenStream, TokenStream)> = Vec::with_capacity(accepting_states.len());
    let mut default = default_rhs;

    for AcceptingState { value, right_ctx } in accepting_states {
        let action_code = generate_rhs_code(ctx, *value);
        match right_ctx {
            Some(right_ctx) => {
                let right_ctx_fn = right_ctx_fn_name(ctx.lexer_name(), right_ctx);
                alts.push((quote!(#right_ctx_fn(self.0.__iter.clone())), action_code));
            }
            None => {
                default = action_code;
                break;
            }
        }
    }

    let mut action_code = default;

    for (cond, rhs) in alts.into_iter().rev() {
        action_code = quote!(if #cond { #rhs } else { #action_code });
    }

    action_code
}

fn inclusive_range_contains(value: TokenStream, range_start: char, range_end: char) -> TokenStream {
    if range_start == range_end {
        quote!(#value == #range_start)
    } else {
        quote!((#range_start..=#range_end).contains(&#value))
    }
}
