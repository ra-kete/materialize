// Copyright Syn Developers.
// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// This file is derived from the syn project, available at
// https://github.com/dtolnay/syn. It was incorporated
// directly into Materialize on January 22, 2021.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License in the LICENSE file at the
// root of this repository, or online at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Transformation of an owned AST.
//!
//! Each method of the [`Fold`] trait is a hook that can be overridden to
//! customize the behavior when transforming the corresponding type of node. By
//! default, every method recursively transforms the substructure of the input
//! by invoking the right folder method on each of its fields.
//!
//! ```
//! # use mz_sql_parser::ast::{Expr, Function, FunctionArgs, UnresolvedObjectName, WindowSpec, Raw, AstInfo};
//! #
//! pub trait Fold<T: AstInfo, T2: AstInfo> {
//!     /* ... */
//!
//!     fn fold_function(&mut self, node: Function<T>) -> Function<T2> {
//!         fold_function(self, node)
//!     }
//!
//!     /* ... */
//!     # fn fold_unresolved_object_name(&mut self, node: UnresolvedObjectName) -> UnresolvedObjectName;
//!     # fn fold_function_args(&mut self, node: FunctionArgs<T>) -> FunctionArgs<T2>;
//!     # fn fold_expr(&mut self, node: Expr<T>) -> Expr<T2>;
//!     # fn fold_window_spec(&mut self, node: WindowSpec<T>) -> WindowSpec<T2>;
//! }
//!
//! pub fn fold_function<F, T: AstInfo, T2: AstInfo>(folder: &mut F, node: Function<T>) -> Function<T2>
//! where
//!     F: Fold<T, T2> + ?Sized,
//! {
//!     Function {
//!         name: folder.fold_unresolved_object_name(node.name),
//!         args: folder.fold_function_args(node.args),
//!         filter: node.filter.map(|filter| Box::new(folder.fold_expr(*filter))),
//!         over: node.over.map(|over| folder.fold_window_spec(over)),
//!         distinct: node.distinct,
//!    }
//! }
//! ```
//!
//! Of particular note to the fold transformation is its handling of the AST's
//! generic parameter. The `Fold` trait is defined so that references to `T`
//! in the input are replaced by references to `T2` in the output. If
//! transformation of `T` is not required, implement `Fold` such that `T` and
//! `T2` refer to the same concrete type, and then provide trivial
//! implementations of any methods that fold `T`'s associated types.
//!
//! The [`FoldNode`] trait is implemented for every node in the AST and can be
//! used to write generic functions that apply a `Fold` implementation to any
//! node in the AST.
//!
//! # Implementation notes
//!
//! This module is automatically generated by the crate's build script. Changes
//! to the AST will be automatically propagated to the fold transformation.
//!
//! This approach to AST transformations is inspired by the [`syn`] crate. These
//! module docs are directly derived from the [`syn::fold`] module docs.
//!
//! [`syn`]: https://docs.rs/syn/1.*/syn/index.html
//! [`syn::fold`]: https://docs.rs/syn/1.*/syn/fold/index.html

#![allow(clippy::all)]
#![allow(unused_variables)]

use super::*;

include!(concat!(env!("OUT_DIR"), "/fold.rs"));
