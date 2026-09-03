//! The abstract syntax tree. Deliberately small: one `Stmt` enum and one `Expr` enum, with shared
//! sub-structures for functions and patterns. The interpreter walks this tree directly.

use std::rc::Rc;

pub type P<T> = Box<T>;

#[derive(Debug, Clone)]
pub enum Stmt {
    Expr(Expr),
    /// `var` / `let` / `const` declaration: kind + (target, optional initializer) pairs.
    VarDecl {
        kind: DeclKind,
        decls: Vec<(Pattern, Option<Expr>)>,
    },
    FuncDecl(Rc<Function>),
    Return(Option<Expr>),
    If {
        test: Expr,
        cons: P<Stmt>,
        alt: Option<P<Stmt>>,
    },
    Block(Vec<Stmt>),
    While {
        test: Expr,
        body: P<Stmt>,
    },
    DoWhile {
        body: P<Stmt>,
        test: Expr,
    },
    /// C-style `for (init; test; update) body`.
    For {
        init: Option<P<ForInit>>,
        test: Option<Expr>,
        update: Option<Expr>,
        body: P<Stmt>,
    },
    /// `for (left in right) body` / `for (left of right) body` (`is_await` for `for await … of`).
    ForInOf {
        decl: Option<DeclKind>,
        left: Pattern,
        right: Expr,
        of: bool,
        is_await: bool,
        body: P<Stmt>,
    },
    Break(Option<String>),
    Continue(Option<String>),
    Throw(Expr),
    Try {
        block: Vec<Stmt>,
        handler: Option<(Option<Pattern>, Vec<Stmt>)>,
        finalizer: Option<Vec<Stmt>>,
    },
    Switch {
        disc: Expr,
        cases: Vec<SwitchCase>,
    },
    Labeled {
        label: String,
        body: P<Stmt>,
    },
    /// `with (obj) body` — resolves identifiers against `obj` first (forbidden in strict mode).
    With {
        obj: Expr,
        body: P<Stmt>,
    },
    ClassDecl(Rc<Class>),
    Empty,
    Debugger,
    /// `import …from "spec"` (or a bare `import "spec"`).
    Import(ImportDecl),
    /// `export { a, b as c }` or `export { a } from "spec"`.
    ExportNamed {
        specs: Vec<ExportSpec>,
        source: Option<Rc<str>>,
    },
    /// `export const/let/var/function/class …` — the inner declaration plus its exported names.
    ExportDecl(P<Stmt>),
    /// `export default …` (expression, function, or class).
    ExportDefault(P<Stmt>),
    /// `export * from "spec"` or `export * as ns from "spec"`.
    ExportAll {
        source: Rc<str>,
        exported: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct ImportDecl {
    pub source: Rc<str>,
    pub specs: Vec<ImportSpec>,
    /// The `with { type: "..." }` import attribute (json/text/bytes), if present.
    pub attr_type: Option<String>,
}
#[derive(Debug, Clone)]
pub enum ImportSpec {
    /// `import x from "…"`
    Default(String),
    /// `import * as ns from "…"`
    Namespace(String),
    /// `import defer * as ns from "…"` — evaluation deferred until the namespace is accessed.
    DeferNamespace(String),
    /// `import source x from "…"` — a source-phase import binding the module's ModuleSource.
    Source(String),
    /// `import { imported as local } from "…"`
    Named { imported: String, local: String },
}
#[derive(Debug, Clone)]
pub struct ExportSpec {
    pub local: String,
    pub exported: String,
}

#[derive(Debug, Clone)]
pub struct Class {
    pub name: Option<String>,
    pub superclass: Option<P<Expr>>,
    pub members: Vec<ClassMember>,
    /// `@dec` decorators applied to the whole class (outermost last).
    pub decorators: Vec<Expr>,
    /// The class's source text (what the constructor's `toString` returns).
    pub source: Option<Rc<str>>,
}

#[derive(Debug, Clone)]
pub struct ClassMember {
    pub key: PropKey,
    pub kind: MemberKind,
    pub is_static: bool,
    /// For methods/accessors/constructor.
    pub func: Option<Rc<Function>>,
    /// For fields (`x = init` / `x`).
    pub value: Option<Expr>,
    /// `@dec` decorators applied to this element.
    pub decorators: Vec<Expr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberKind {
    Constructor,
    Method,
    Get,
    Set,
    Field,
    /// `accessor x = init` — an auto-accessor: a private backing field plus a getter/setter pair.
    Accessor,
    /// `static { ... }` — runs once at class definition with `this` = the class.
    StaticBlock,
}

#[derive(Debug, Clone)]
pub enum ForInit {
    VarDecl {
        kind: DeclKind,
        decls: Vec<(Pattern, Option<Expr>)>,
    },
    Expr(Expr),
}

#[derive(Debug, Clone)]
pub struct SwitchCase {
    /// `None` is the `default:` clause.
    pub test: Option<Expr>,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclKind {
    Var,
    Let,
    Const,
    /// `using x = expr;` — a block-scoped binding disposed (`[Symbol.dispose]()`) at scope exit.
    Using,
    /// `await using x = expr;` — disposed via `[Symbol.asyncDispose]()` (awaited) at scope exit.
    AwaitUsing,
}

#[derive(Debug, Clone)]
pub enum Pattern {
    Ident(String),
    /// `[a, b = 1, ...rest]` — elements may be holes, may carry defaults, and the last may be a rest.
    Array(Vec<ArrayPatElem>),
    /// `{ a, b: x = 1, ...rest }`.
    Object(ObjectPat),
    /// A member-expression assignment target (`o.p`, `o[k]`) — only valid in assignment-style
    /// destructuring / `for (o.p of …)`, never in a declaration.
    Member(Box<Expr>),
}

#[derive(Debug, Clone)]
pub enum ArrayPatElem {
    Hole,
    Elem {
        pattern: Pattern,
        default: Option<Expr>,
    },
    Rest(Pattern),
}

#[derive(Debug, Clone)]
pub struct ObjectPat {
    pub props: Vec<ObjPatProp>,
    /// `...rest` — a plain identifier collecting the remaining own enumerable keys.
    pub rest: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ObjPatProp {
    pub key: PropKey,
    pub value: Pattern,
    pub default: Option<Expr>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // some node fields (regex body/flags) are parsed before they are interpreted
pub enum Expr {
    /// A parenthesized array/object literal or assignment — recorded so destructuring
    /// reinterpretation can reject it (parens block the pattern refinement); evaluates
    /// transparently.
    Paren(P<Expr>),
    Num(f64),
    BigInt(crate::bigint::JsBigInt),
    Str(Rc<str>),
    /// A template-literal substitution: evaluate the inner expression and apply ToString (which uses
    /// the `string` hint — toString before valueOf — unlike `+` which uses the `default` hint).
    ToStr(Box<Expr>),
    Bool(bool),
    Null,
    Undefined,
    Ident(String),
    This,
    Regex {
        body: Rc<str>,
        flags: Rc<str>,
    },
    Array(Vec<ArrayElem>),
    Object(Vec<PropDef>),
    Func(Rc<Function>),
    Class(Rc<Class>),
    /// `yield expr` / `yield* expr` (only inside a generator).
    Yield {
        delegate: bool,
        arg: Option<P<Expr>>,
    },
    /// `await expr` (only inside an async function).
    Await(P<Expr>),
    /// The bare `super` keyword (only valid as `super(...)` or `super.x` / `super[x]`).
    Super,
    Unary {
        op: &'static str,
        arg: P<Expr>,
    },
    Update {
        op: &'static str,
        prefix: bool,
        arg: P<Expr>,
    },
    Binary {
        op: &'static str,
        left: P<Expr>,
        right: P<Expr>,
    },
    Logical {
        op: &'static str,
        left: P<Expr>,
        right: P<Expr>,
    },
    Assign {
        op: &'static str,
        target: P<Expr>,
        value: P<Expr>,
    },
    Cond {
        test: P<Expr>,
        cons: P<Expr>,
        alt: P<Expr>,
    },
    Call {
        callee: P<Expr>,
        args: Vec<ArrayElem>,
        optional: bool,
    },
    New {
        callee: P<Expr>,
        args: Vec<ArrayElem>,
    },
    Member {
        obj: P<Expr>,
        prop: String,
        optional: bool,
    },
    Index {
        obj: P<Expr>,
        index: P<Expr>,
        optional: bool,
    },
    Seq(Vec<Expr>),
    /// `tag\`a${x}b\`` — `quasis` are (cooked, raw) chunks (one more than `subs`).
    TaggedTemplate {
        tag: P<Expr>,
        /// Stable Parse Node identity for ECMA-262's per-Realm [[TemplateMap]].
        site: u64,
        quasis: Vec<(Option<String>, String)>,
        subs: Vec<Expr>,
    },
    /// An optional chain (`a?.b.c`): evaluates the inner LHS, short-circuiting to `undefined` if any
    /// `?.` link sees a nullish base.
    OptionalChain(P<Expr>),
    /// Ergonomic brand check `#field in obj`: whether `obj` carries the private field.
    PrivateIn {
        name: String,
        obj: P<Expr>,
    },
    /// Dynamic `import(specifier)` / `import.source(...)` / `import.defer(...)` — returns a
    /// promise. The phase selects the import semantics.
    ImportCall {
        spec: P<Expr>,
        phase: ImportPhase,
        /// The optional second argument (`import(spec, { with: { type: "json" } })`).
        options: Option<P<Expr>>,
    },
    /// `import.meta`.
    ImportMeta,
    /// `new.target`.
    NewTarget,
}

/// The phase of a dynamic `import()` call: plain evaluation, `import.source(...)` (source-phase),
/// or `import.defer(...)` (deferred-evaluation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportPhase {
    Evaluation,
    Source,
    Defer,
}

/// An array element or call argument: a value, a spread (`...x`), or a hole (`[1,,3]`).
#[derive(Debug, Clone)]
pub enum ArrayElem {
    Item(Expr),
    Spread(Expr),
    Hole,
}

#[derive(Debug, Clone)]
pub enum PropDef {
    /// `key: value` or shorthand `{ x }`.
    KeyValue {
        key: PropKey,
        value: Expr,
    },
    /// CoverInitializedName (`{ x = default }`): only valid when the literal is reinterpreted as
    /// a destructuring pattern; the parser rejects it anywhere else. `value` is the
    /// `x = default` assignment.
    Cover {
        key: PropKey,
        value: Expr,
    },
    /// Concise method `key() {}` (incl. generator/async). Carries a [[HomeObject]] so `super`
    /// inside the body resolves against the literal's prototype.
    Method {
        key: PropKey,
        func: Rc<Function>,
    },
    /// `get key() {}` / `set key(v) {}`.
    Getter {
        key: PropKey,
        func: Rc<Function>,
    },
    Setter {
        key: PropKey,
        func: Rc<Function>,
    },
    Spread(Expr),
    /// The colon-form `__proto__: value` in an object literal — sets `[[Prototype]]` (when the
    /// value is an Object or Null) rather than creating a property. Only the non-computed,
    /// non-shorthand, non-method form. As a destructuring pattern it degrades to a normal
    /// `__proto__` keyed target.
    Proto(Expr),
}

#[derive(Debug, Clone)]
pub enum PropKey {
    Ident(String),
    Str(Rc<str>),
    Num(f64),
    Computed(Expr),
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // expr_body is recorded for a future `toString`/source-fidelity pass
pub struct Function {
    pub name: Option<String>,
    pub params: Vec<Param>,
    pub body: Vec<Stmt>,
    pub is_arrow: bool,
    pub is_strict: bool,
    /// Arrow with an expression body (`x => x+1`): the single statement is a synthetic `return`.
    pub expr_body: bool,
    pub is_generator: bool,
    pub is_async: bool,
    /// A concise method / getter / setter: has no own `prototype` and is not a constructor (the
    /// class `constructor` member is re-flagged false once identified).
    pub is_method: bool,
    /// A function *expression* (`(function f(){})`): its own name binds immutably inside the
    /// function. A declaration's name binds (mutably) in the enclosing scope instead.
    pub is_fn_expr: bool,
    /// The synthesized default constructor of a class (ECMA-262 ClassDefinitionEvaluation,
    /// `ClassTail`: default constructors forward the raw argument list to `super` instead of
    /// performing the observable `%Array.prototype%` iterator evaluation of a written
    /// `constructor(...args) { super(...args); }`; see `default_constructor`). Always false for
    /// parsed source; only the class member synthesis sets it.
    pub default_ctor: bool,
    /// The source text this function was parsed from, for `Function.prototype.toString`.
    pub source: Option<Rc<str>>,
    /// Lazily-computed body facts (see [`Function::scan_flags`]): bit 0 = scanned, bit 1 =
    /// references `arguments`, bit 2 = references `new.target`, bit 3 = references `this`.
    /// A direct `eval` sets all three (it can reach any of them dynamically).
    pub scan: std::cell::Cell<u8>,
    /// Lazily-computed hoisting plan for the body (the strict flag it was computed under plus the
    /// ops), so calls replay a flat list instead of re-walking the AST (see
    /// `interpreter::collect_hoist_ops`).
    pub hoist: std::cell::OnceCell<(bool, Rc<Vec<HoistOp>>)>,
    /// Bytecode-tier state: call count until tier-up, and the compile result once attempted
    /// (`None` = uses constructs outside the bytecode subset; runs in the tree-walker forever).
    pub calls: std::cell::Cell<u32>,
    pub code: std::cell::OnceCell<Option<Rc<crate::bytecode::Chunk>>>,
    /// Second-stage compile: the body re-compiled with hot monomorphic callees inlined (see
    /// `bytecode::plan_inlines`). Preferred over `code` wherever a chunk is fetched; `code`
    /// stays alive because filled call ICs hold raw pointers into it.
    pub code2: std::cell::OnceCell<Option<Rc<crate::bytecode::Chunk>>>,
    /// Lazily-built object-map templates for closures of this function (see
    /// `Interp::make_function`): the function object's `Props` (length/name and a `prototype`
    /// placeholder) and, for prototype-bearing kinds, the fresh `.prototype`'s `Props` — cloned
    /// per closure instance instead of rebuilt insert by insert, so key hashing and shape
    /// transitions are paid once per FUNCTION rather than once per closure.
    pub fn_maps: std::cell::OnceCell<(crate::value::Props, Option<crate::value::Props>)>,
}

/// One pre-scanned hoisting action for a statement list, replayed against a scope at function
/// call / script entry (see `interpreter::collect_hoist_ops` — order matters: `var`s in
/// traversal order, then function declarations in source order, then Annex B promotions).
#[derive(Debug, Clone)]
pub enum HoistOp {
    /// Declare a `var` binding (undefined) unless the name is already bound.
    Var(String),
    /// Bind a hoisted function declaration (`*default*` names as "default").
    Fn(String, Rc<Function>),
    /// Annex B.3.3: promote a sloppy block function to an (if-absent) var binding and register
    /// it for declaration-time sync.
    AnnexB(String, Rc<Function>),
}

struct RetainedAst<'a> {
    visitor: &'a mut crate::memory::Visitor,
    bytes: usize,
}

impl RetainedAst<'_> {
    fn add(&mut self, bytes: usize) {
        self.bytes = self.bytes.saturating_add(bytes);
    }

    fn vec<T>(&mut self, values: &Vec<T>) {
        self.add(values.capacity().saturating_mul(std::mem::size_of::<T>()));
    }

    fn string(&mut self, value: &String) {
        self.add(value.capacity());
    }

    fn rc_str(&mut self, value: &Rc<str>) {
        self.visitor.rc_str(value);
    }

    fn boxed_expr(&mut self, value: &Expr) {
        self.add(std::mem::size_of::<Expr>());
        self.expr(value);
    }

    fn boxed_stmt(&mut self, value: &Stmt) {
        self.add(std::mem::size_of::<Stmt>());
        self.stmt(value);
    }

    fn stmt_vec(&mut self, values: &Vec<Stmt>) {
        self.vec(values);
        for value in values {
            self.stmt(value);
        }
    }

    fn decls(&mut self, decls: &Vec<(Pattern, Option<Expr>)>) {
        self.vec(decls);
        for (pattern, initializer) in decls {
            self.pattern(pattern);
            if let Some(initializer) = initializer {
                self.expr(initializer);
            }
        }
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Expr(expr) | Stmt::Throw(expr) => self.expr(expr),
            Stmt::VarDecl { kind: _, decls } => self.decls(decls),
            Stmt::FuncDecl(function) => self.visitor.function(function),
            Stmt::Return(expr) => {
                if let Some(expr) = expr {
                    self.expr(expr);
                }
            }
            Stmt::If { test, cons, alt } => {
                self.expr(test);
                self.boxed_stmt(cons);
                if let Some(alt) = alt {
                    self.boxed_stmt(alt);
                }
            }
            Stmt::Block(body) => self.stmt_vec(body),
            Stmt::While { test, body } => {
                self.expr(test);
                self.boxed_stmt(body);
            }
            Stmt::DoWhile { body, test } => {
                self.boxed_stmt(body);
                self.expr(test);
            }
            Stmt::For {
                init,
                test,
                update,
                body,
            } => {
                if let Some(init) = init {
                    self.add(std::mem::size_of::<ForInit>());
                    self.for_init(init);
                }
                if let Some(test) = test {
                    self.expr(test);
                }
                if let Some(update) = update {
                    self.expr(update);
                }
                self.boxed_stmt(body);
            }
            Stmt::ForInOf {
                decl: _,
                left,
                right,
                of: _,
                is_await: _,
                body,
            } => {
                self.pattern(left);
                self.expr(right);
                self.boxed_stmt(body);
            }
            Stmt::Break(label) | Stmt::Continue(label) => {
                if let Some(label) = label {
                    self.string(label);
                }
            }
            Stmt::Try {
                block,
                handler,
                finalizer,
            } => {
                self.stmt_vec(block);
                if let Some((pattern, body)) = handler {
                    if let Some(pattern) = pattern {
                        self.pattern(pattern);
                    }
                    self.stmt_vec(body);
                }
                if let Some(finalizer) = finalizer {
                    self.stmt_vec(finalizer);
                }
            }
            Stmt::Switch { disc, cases } => {
                self.expr(disc);
                self.vec(cases);
                for case in cases {
                    if let Some(test) = &case.test {
                        self.expr(test);
                    }
                    self.stmt_vec(&case.body);
                }
            }
            Stmt::Labeled { label, body } => {
                self.string(label);
                self.boxed_stmt(body);
            }
            Stmt::With { obj, body } => {
                self.expr(obj);
                self.boxed_stmt(body);
            }
            Stmt::ClassDecl(class) => self.visitor.class(class),
            Stmt::Empty | Stmt::Debugger => {}
            Stmt::Import(import) => self.import(import),
            Stmt::ExportNamed { specs, source } => {
                self.vec(specs);
                for spec in specs {
                    self.string(&spec.local);
                    self.string(&spec.exported);
                }
                if let Some(source) = source {
                    self.rc_str(source);
                }
            }
            Stmt::ExportDecl(inner) | Stmt::ExportDefault(inner) => self.boxed_stmt(inner),
            Stmt::ExportAll { source, exported } => {
                self.rc_str(source);
                if let Some(exported) = exported {
                    self.string(exported);
                }
            }
        }
    }

    fn import(&mut self, import: &ImportDecl) {
        self.rc_str(&import.source);
        self.vec(&import.specs);
        for spec in &import.specs {
            match spec {
                ImportSpec::Default(local)
                | ImportSpec::Namespace(local)
                | ImportSpec::DeferNamespace(local)
                | ImportSpec::Source(local) => self.string(local),
                ImportSpec::Named { imported, local } => {
                    self.string(imported);
                    self.string(local);
                }
            }
        }
        if let Some(attr_type) = &import.attr_type {
            self.string(attr_type);
        }
    }

    fn for_init(&mut self, init: &ForInit) {
        match init {
            ForInit::VarDecl { kind: _, decls } => self.decls(decls),
            ForInit::Expr(expr) => self.expr(expr),
        }
    }

    fn pattern(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::Ident(name) => self.string(name),
            Pattern::Array(elements) => {
                self.vec(elements);
                for element in elements {
                    match element {
                        ArrayPatElem::Hole => {}
                        ArrayPatElem::Elem { pattern, default } => {
                            self.pattern(pattern);
                            if let Some(default) = default {
                                self.expr(default);
                            }
                        }
                        ArrayPatElem::Rest(pattern) => self.pattern(pattern),
                    }
                }
            }
            Pattern::Object(object) => {
                self.vec(&object.props);
                for property in &object.props {
                    self.prop_key(&property.key);
                    self.pattern(&property.value);
                    if let Some(default) = &property.default {
                        self.expr(default);
                    }
                }
                if let Some(rest) = &object.rest {
                    self.string(rest);
                }
            }
            Pattern::Member(expr) => self.boxed_expr(expr),
        }
    }

    fn expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Paren(inner)
            | Expr::ToStr(inner)
            | Expr::Await(inner)
            | Expr::OptionalChain(inner) => self.boxed_expr(inner),
            Expr::Num(_)
            | Expr::Bool(_)
            | Expr::Null
            | Expr::Undefined
            | Expr::This
            | Expr::Super
            | Expr::ImportMeta
            | Expr::NewTarget => {}
            Expr::BigInt(value) => self.visitor.bigint(value),
            Expr::Str(value) => self.rc_str(value),
            Expr::Ident(name) => self.string(name),
            Expr::Regex { body, flags } => {
                self.rc_str(body);
                self.rc_str(flags);
            }
            Expr::Array(elements) => {
                self.vec(elements);
                for element in elements {
                    self.array_elem(element);
                }
            }
            Expr::Object(properties) => {
                self.vec(properties);
                for property in properties {
                    self.prop_def(property);
                }
            }
            Expr::Func(function) => self.visitor.function(function),
            Expr::Class(class) => self.visitor.class(class),
            Expr::Yield { delegate: _, arg } => {
                if let Some(arg) = arg {
                    self.boxed_expr(arg);
                }
            }
            Expr::Unary { op: _, arg }
            | Expr::Update {
                op: _,
                prefix: _,
                arg,
            } => self.boxed_expr(arg),
            Expr::Binary { op: _, left, right }
            | Expr::Logical { op: _, left, right }
            | Expr::Assign {
                op: _,
                target: left,
                value: right,
            } => {
                self.boxed_expr(left);
                self.boxed_expr(right);
            }
            Expr::Cond { test, cons, alt } => {
                self.boxed_expr(test);
                self.boxed_expr(cons);
                self.boxed_expr(alt);
            }
            Expr::Call {
                callee,
                args,
                optional: _,
            }
            | Expr::New { callee, args } => {
                self.boxed_expr(callee);
                self.vec(args);
                for argument in args {
                    self.array_elem(argument);
                }
            }
            Expr::Member {
                obj,
                prop,
                optional: _,
            } => {
                self.boxed_expr(obj);
                self.string(prop);
            }
            Expr::Index {
                obj,
                index,
                optional: _,
            } => {
                self.boxed_expr(obj);
                self.boxed_expr(index);
            }
            Expr::Seq(expressions) => {
                self.vec(expressions);
                for expression in expressions {
                    self.expr(expression);
                }
            }
            Expr::TaggedTemplate {
                tag,
                site: _,
                quasis,
                subs,
            } => {
                self.boxed_expr(tag);
                self.vec(quasis);
                for (cooked, raw) in quasis {
                    if let Some(cooked) = cooked {
                        self.string(cooked);
                    }
                    self.string(raw);
                }
                self.vec(subs);
                for substitution in subs {
                    self.expr(substitution);
                }
            }
            Expr::PrivateIn { name, obj } => {
                self.string(name);
                self.boxed_expr(obj);
            }
            Expr::ImportCall {
                spec,
                phase: _,
                options,
            } => {
                self.boxed_expr(spec);
                if let Some(options) = options {
                    self.boxed_expr(options);
                }
            }
        }
    }

    fn array_elem(&mut self, element: &ArrayElem) {
        match element {
            ArrayElem::Item(expr) | ArrayElem::Spread(expr) => self.expr(expr),
            ArrayElem::Hole => {}
        }
    }

    fn prop_def(&mut self, property: &PropDef) {
        match property {
            PropDef::KeyValue { key, value } | PropDef::Cover { key, value } => {
                self.prop_key(key);
                self.expr(value);
            }
            PropDef::Method { key, func }
            | PropDef::Getter { key, func }
            | PropDef::Setter { key, func } => {
                self.prop_key(key);
                self.visitor.function(func);
            }
            PropDef::Spread(expr) | PropDef::Proto(expr) => self.expr(expr),
        }
    }

    fn prop_key(&mut self, key: &PropKey) {
        match key {
            PropKey::Ident(name) => self.string(name),
            PropKey::Str(value) => self.rc_str(value),
            PropKey::Num(_) => {}
            PropKey::Computed(expr) => self.expr(expr),
        }
    }

    fn class(&mut self, class: &Class) {
        if let Some(name) = &class.name {
            self.string(name);
        }
        if let Some(superclass) = &class.superclass {
            self.boxed_expr(superclass);
        }
        self.vec(&class.members);
        for member in &class.members {
            self.prop_key(&member.key);
            if let Some(function) = &member.func {
                self.visitor.function(function);
            }
            if let Some(value) = &member.value {
                self.expr(value);
            }
            self.vec(&member.decorators);
            for decorator in &member.decorators {
                self.expr(decorator);
            }
        }
        self.vec(&class.decorators);
        for decorator in &class.decorators {
            self.expr(decorator);
        }
        if let Some(source) = &class.source {
            self.rc_str(source);
        }
    }
}

/// Scan one identity-unique Function allocation and every non-shared AST allocation it owns.
/// Rc-backed Functions, Classes, strings, and BigInts route through the global allocation-family
/// visitor so shared subgraphs are never attributed according to discovery order.
pub(crate) fn scan_function_retained_memory(
    function: &Function,
    visitor: &mut crate::memory::Visitor,
) -> usize {
    let mut scan = RetainedAst {
        visitor,
        bytes: std::mem::size_of::<Function>(),
    };
    if let Some(name) = &function.name {
        scan.string(name);
    }
    scan.vec(&function.params);
    for param in &function.params {
        scan.pattern(&param.pattern);
        if let Some(default) = &param.default {
            scan.expr(default);
        }
    }
    scan.stmt_vec(&function.body);
    if let Some(source) = &function.source {
        scan.rc_str(source);
    }
    scan.bytes
}

/// Scan one identity-unique Class allocation and its recursively-owned AST storage.
pub(crate) fn scan_class_retained_memory(
    class: &Class,
    visitor: &mut crate::memory::Visitor,
) -> usize {
    let mut scan = RetainedAst {
        visitor,
        bytes: std::mem::size_of::<Class>(),
    };
    scan.class(class);
    scan.bytes
}

/// Scan allocations recursively owned by an Expr whose enum body is stored inline by its caller.
pub(crate) fn scan_expr_retained_memory(
    expr: &Expr,
    visitor: &mut crate::memory::Visitor,
) -> usize {
    let mut scan = RetainedAst { visitor, bytes: 0 };
    scan.expr(expr);
    scan.bytes
}

/// Scan an Rc-owned statement-list allocation whose Vec header and element buffer are not part of
/// a Function body allocation.
pub(crate) fn scan_stmt_body_retained_memory(
    body: &Vec<Stmt>,
    visitor: &mut crate::memory::Visitor,
) -> usize {
    let mut scan = RetainedAst {
        visitor,
        bytes: std::mem::size_of::<Vec<Stmt>>(),
    };
    scan.stmt_vec(body);
    scan.bytes
}

pub const SCAN_DONE: u8 = 1;
pub const SCAN_ARGUMENTS: u8 = 2;
pub const SCAN_NEW_TARGET: u8 = 4;
pub const SCAN_THIS: u8 = 8;
/// The body itself contains a loop statement (not counting nested functions): the bytecode tier
/// compiles such functions on their *first* call — a single call can run a million iterations
/// (a benchmark driver's `while (elapsed < 1000)`), so waiting for a call-count threshold leaves
/// the hottest code on the tree-walker.
pub const SCAN_HAS_LOOP: u8 = 16;

impl Function {
    /// What this function's own activation must provide: whether the body (or a nested arrow, or a
    /// possible direct `eval`) can observe `arguments`, `new.target`, or `this`. Ordinary nested
    /// functions are opaque (they get their own); arrows are transparent. Conservative on the
    /// safe side: a false positive only costs an unused binding.
    pub fn scan_flags(&self) -> u8 {
        let cached = self.scan.get();
        if cached & SCAN_DONE != 0 {
            return cached;
        }
        let mut flags = SCAN_DONE;
        for p in &self.params {
            scan_pattern(&p.pattern, &mut flags);
            if let Some(d) = &p.default {
                scan_expr(d, &mut flags);
            }
        }
        scan_stmts(&self.body, &mut flags);
        self.scan.set(flags);
        flags
    }
}

const SCAN_ALL: u8 = SCAN_DONE | SCAN_ARGUMENTS | SCAN_NEW_TARGET | SCAN_THIS | SCAN_HAS_LOOP;

fn scan_stmts(body: &[Stmt], flags: &mut u8) {
    for s in body {
        scan_stmt(s, flags);
        if *flags == SCAN_ALL {
            return;
        }
    }
}

fn scan_stmt(s: &Stmt, flags: &mut u8) {
    match s {
        Stmt::Expr(e) | Stmt::Throw(e) => scan_expr(e, flags),
        Stmt::VarDecl { kind: _, decls } => {
            for (pat, init) in decls {
                scan_pattern(pat, flags);
                if let Some(e) = init {
                    scan_expr(e, flags);
                }
            }
        }
        // A nested (non-arrow) function has its own arguments/new.target/this.
        Stmt::FuncDecl(_) => {}
        Stmt::Return(e) => {
            if let Some(e) = e {
                scan_expr(e, flags);
            }
        }
        Stmt::If { test, cons, alt } => {
            scan_expr(test, flags);
            scan_stmt(cons, flags);
            if let Some(a) = alt {
                scan_stmt(a, flags);
            }
        }
        Stmt::Block(b) => scan_stmts(b, flags),
        Stmt::While { test, body } | Stmt::DoWhile { body, test } => {
            *flags |= SCAN_HAS_LOOP;
            scan_expr(test, flags);
            scan_stmt(body, flags);
        }
        Stmt::For {
            init,
            test,
            update,
            body,
        } => {
            *flags |= SCAN_HAS_LOOP;
            match init.as_deref() {
                Some(ForInit::VarDecl { kind: _, decls }) => {
                    for (pat, e) in decls {
                        scan_pattern(pat, flags);
                        if let Some(e) = e {
                            scan_expr(e, flags);
                        }
                    }
                }
                Some(ForInit::Expr(e)) => scan_expr(e, flags),
                None => {}
            }
            if let Some(e) = test {
                scan_expr(e, flags);
            }
            if let Some(e) = update {
                scan_expr(e, flags);
            }
            scan_stmt(body, flags);
        }
        Stmt::ForInOf {
            decl: _,
            left,
            right,
            of: _,
            is_await: _,
            body,
        } => {
            *flags |= SCAN_HAS_LOOP;
            scan_pattern(left, flags);
            scan_expr(right, flags);
            scan_stmt(body, flags);
        }
        Stmt::Break(_) | Stmt::Continue(_) | Stmt::Empty | Stmt::Debugger => {}
        Stmt::Try {
            block,
            handler,
            finalizer,
        } => {
            scan_stmts(block, flags);
            if let Some((param, hbody)) = handler {
                if let Some(p) = param {
                    scan_pattern(p, flags);
                }
                scan_stmts(hbody, flags);
            }
            if let Some(f) = finalizer {
                scan_stmts(f, flags);
            }
        }
        Stmt::Switch { disc, cases } => {
            scan_expr(disc, flags);
            for c in cases {
                if let Some(t) = &c.test {
                    scan_expr(t, flags);
                }
                scan_stmts(&c.body, flags);
            }
        }
        Stmt::Labeled { label: _, body } => scan_stmt(body, flags),
        Stmt::With { obj, body } => {
            scan_expr(obj, flags);
            scan_stmt(body, flags);
        }
        Stmt::ClassDecl(c) => scan_class(c, flags),
        Stmt::Import(_) | Stmt::ExportNamed { .. } | Stmt::ExportAll { .. } => {}
        Stmt::ExportDecl(inner) | Stmt::ExportDefault(inner) => scan_stmt(inner, flags),
    }
}

fn scan_expr(e: &Expr, flags: &mut u8) {
    match e {
        Expr::Ident(n) => {
            if n == "arguments" {
                *flags |= SCAN_ARGUMENTS;
            }
        }
        Expr::This => *flags |= SCAN_THIS,
        Expr::NewTarget => *flags |= SCAN_NEW_TARGET,
        Expr::Num(_)
        | Expr::BigInt(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Null
        | Expr::Undefined
        | Expr::Regex { .. }
        | Expr::ImportMeta => {}
        // `super.x` resolves its receiver through the `this` binding; `super()` initializes it.
        Expr::Super => *flags |= SCAN_THIS,
        Expr::Paren(inner)
        | Expr::ToStr(inner)
        | Expr::Await(inner)
        | Expr::OptionalChain(inner) => scan_expr(inner, flags),
        Expr::Array(elems) => {
            for el in elems {
                match el {
                    ArrayElem::Item(e) | ArrayElem::Spread(e) => scan_expr(e, flags),
                    ArrayElem::Hole => {}
                }
            }
        }
        Expr::Object(props) => {
            for p in props {
                match p {
                    PropDef::KeyValue { key, value } | PropDef::Cover { key, value } => {
                        scan_prop_key(key, flags);
                        scan_expr(value, flags);
                    }
                    // A concise method/accessor body is its own function scope; only its
                    // (computed) key evaluates here.
                    PropDef::Method { key, func: _ }
                    | PropDef::Getter { key, func: _ }
                    | PropDef::Setter { key, func: _ } => scan_prop_key(key, flags),
                    PropDef::Spread(e) | PropDef::Proto(e) => scan_expr(e, flags),
                }
            }
        }
        // An arrow is transparent (it closes over the enclosing activation); an ordinary
        // function expression is opaque.
        Expr::Func(f) => {
            if f.is_arrow {
                let inner = f.scan_flags();
                *flags |= inner & (SCAN_ARGUMENTS | SCAN_NEW_TARGET | SCAN_THIS);
            }
        }
        Expr::Class(c) => scan_class(c, flags),
        Expr::Yield { delegate: _, arg } => {
            if let Some(a) = arg {
                scan_expr(a, flags);
            }
        }
        Expr::Unary { op: _, arg }
        | Expr::Update {
            op: _,
            prefix: _,
            arg,
        } => scan_expr(arg, flags),
        Expr::Binary { op: _, left, right } | Expr::Logical { op: _, left, right } => {
            scan_expr(left, flags);
            scan_expr(right, flags);
        }
        Expr::Assign {
            op: _,
            target,
            value,
        } => {
            scan_expr(target, flags);
            scan_expr(value, flags);
        }
        Expr::Cond { test, cons, alt } => {
            scan_expr(test, flags);
            scan_expr(cons, flags);
            scan_expr(alt, flags);
        }
        Expr::Call {
            callee,
            args,
            optional: _,
        } => {
            // A direct `eval` can name any of the three dynamically.
            if matches!(&**callee, Expr::Ident(n) if n == "eval") {
                *flags |= SCAN_ARGUMENTS | SCAN_NEW_TARGET | SCAN_THIS;
            }
            // SuperCall begins with GetNewTarget. An arrow is transparent to that lookup, so a
            // constructor containing an async arrow with `super(...)` must retain its own
            // new.target binding after the constructor invocation returns.
            if matches!(&**callee, Expr::Super) {
                *flags |= SCAN_NEW_TARGET;
            }
            scan_expr(callee, flags);
            for a in args {
                match a {
                    ArrayElem::Item(e) | ArrayElem::Spread(e) => scan_expr(e, flags),
                    ArrayElem::Hole => {}
                }
            }
        }
        Expr::New { callee, args } => {
            scan_expr(callee, flags);
            for a in args {
                match a {
                    ArrayElem::Item(e) | ArrayElem::Spread(e) => scan_expr(e, flags),
                    ArrayElem::Hole => {}
                }
            }
        }
        Expr::Member {
            obj,
            prop: _,
            optional: _,
        } => scan_expr(obj, flags),
        Expr::Index {
            obj,
            index,
            optional: _,
        } => {
            scan_expr(obj, flags);
            scan_expr(index, flags);
        }
        Expr::Seq(exprs) => {
            for e in exprs {
                scan_expr(e, flags);
            }
        }
        Expr::TaggedTemplate {
            tag,
            quasis: _,
            subs,
            ..
        } => {
            scan_expr(tag, flags);
            for e in subs {
                scan_expr(e, flags);
            }
        }
        Expr::PrivateIn { name: _, obj } => scan_expr(obj, flags),
        Expr::ImportCall {
            spec,
            phase: _,
            options,
        } => {
            scan_expr(spec, flags);
            if let Some(o) = options {
                scan_expr(o, flags);
            }
        }
    }
}

fn scan_class(c: &Class, flags: &mut u8) {
    // Heritage, decorators, and computed keys evaluate in the enclosing scope. Member bodies are
    // their own function scopes; field/accessor initializers and static blocks can't legally name
    // `arguments`, but walking them costs only a possible false positive.
    if let Some(sc) = &c.superclass {
        scan_expr(sc, flags);
    }
    for d in &c.decorators {
        scan_expr(d, flags);
    }
    for m in &c.members {
        scan_prop_key(&m.key, flags);
        for d in &m.decorators {
            scan_expr(d, flags);
        }
        if let Some(v) = &m.value {
            scan_expr(v, flags);
        }
    }
}

fn scan_prop_key(k: &PropKey, flags: &mut u8) {
    match k {
        PropKey::Ident(_) | PropKey::Str(_) | PropKey::Num(_) => {}
        PropKey::Computed(e) => scan_expr(e, flags),
    }
}

fn scan_pattern(p: &Pattern, flags: &mut u8) {
    match p {
        Pattern::Ident(n) => {
            if n == "arguments" {
                *flags |= SCAN_ARGUMENTS;
            }
        }
        Pattern::Array(elems) => {
            for el in elems {
                match el {
                    ArrayPatElem::Hole => {}
                    ArrayPatElem::Elem { pattern, default } => {
                        scan_pattern(pattern, flags);
                        if let Some(d) = default {
                            scan_expr(d, flags);
                        }
                    }
                    ArrayPatElem::Rest(pat) => scan_pattern(pat, flags),
                }
            }
        }
        Pattern::Object(op) => {
            for prop in &op.props {
                scan_prop_key(&prop.key, flags);
                scan_pattern(&prop.value, flags);
                if let Some(d) = &prop.default {
                    scan_expr(d, flags);
                }
            }
            if op.rest.as_deref() == Some("arguments") {
                *flags |= SCAN_ARGUMENTS;
            }
        }
        Pattern::Member(e) => scan_expr(e, flags),
    }
}

#[derive(Debug, Clone)]
pub struct Param {
    pub pattern: Pattern,
    pub default: Option<Expr>,
    pub rest: bool,
}
