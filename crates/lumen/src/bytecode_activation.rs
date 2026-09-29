//! Fixed captured-binding storage for compiled FunctionDeclarationInstantiation.
//!
//! ECMA-262 #sec-functiondeclarationinstantiation / #sec-declarative-environment-records:
//! binding names and attributes are static; values, TDZ state and function identities
//! are per activation. Compile the creation work once, then seed indexed slots.
//! Prepared entries and reused mapped-arguments environments retain their original
//! instantiation path. Structural eval mutations invalidate VarMap's layout proof.

use super::{CapInit, Chunk};
use crate::interpreter::{
    new_var_scope_with_bindings, Binding, BindingLayout, Env, Interp, VarMap,
};
use crate::value::Value;
use std::rc::Rc;

pub(super) fn activation_plans_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("LUMEN_ACTIVATION_PLANS").as_deref() != Ok("0"))
}

pub(super) struct ActivationPlan {
    bindings: Vec<(Rc<str>, Binding)>,
    layout: Option<Rc<BindingLayout>>,
    parameters: Vec<(usize, u16)>,
    functions: Vec<(usize, u16)>,
    lexicals: Vec<Rc<str>>,
    this_slot: Option<usize>,
    arguments_slot: Option<usize>,
}

impl ActivationPlan {
    pub(super) fn new(chunk: &Chunk) -> Self {
        let mut plan = Self {
            bindings: Vec::with_capacity(chunk.cap_inits.len() + 2),
            layout: None,
            parameters: Vec::new(),
            functions: Vec::new(),
            lexicals: Vec::new(),
            this_slot: None,
            arguments_slot: None,
        };
        let mut slots = crate::fasthash::FastMap::<Rc<str>, usize>::default();
        fn slot(
            bindings: &mut Vec<(Rc<str>, Binding)>,
            slots: &mut crate::fasthash::FastMap<Rc<str>, usize>,
            name: Rc<str>,
            binding: Binding,
            replace: bool,
        ) -> usize {
            if let Some(&slot) = slots.get(&name) {
                if replace {
                    bindings[slot].1 = binding;
                }
                return slot;
            }
            let index = bindings.len();
            slots.insert(name.clone(), index);
            bindings.push((name, binding));
            index
        }
        let mut functions = Vec::new();
        for init in &chunk.cap_inits {
            match init {
                CapInit::Param(parameter, name) => {
                    let index = slot(
                        &mut plan.bindings,
                        &mut slots,
                        name.clone(),
                        Binding::data(Value::Undefined, true, true),
                        true,
                    );
                    plan.parameters.push((index, *parameter));
                }
                CapInit::Var(name) => {
                    slot(
                        &mut plan.bindings,
                        &mut slots,
                        name.clone(),
                        Binding::data(Value::Undefined, true, true),
                        false,
                    );
                }
                CapInit::Lexical(name, is_const) => {
                    slot(
                        &mut plan.bindings,
                        &mut slots,
                        name.clone(),
                        Binding::data(Value::Undefined, !is_const, false),
                        true,
                    );
                    plan.lexicals.push(name.clone());
                }
                CapInit::Fn(function, name) => functions.push((*function, name.clone())),
            }
        }
        if chunk.env_this && !chunk.lexical_this {
            plan.this_slot = Some(slot(
                &mut plan.bindings,
                &mut slots,
                Rc::from("this"),
                Binding::data(Value::Undefined, false, true),
                true,
            ));
        }
        // Source-order function creation follows parameter/var instantiation.
        // Preserve all creations, including source-order replacement of names.
        for (function, name) in functions {
            let index = slot(
                &mut plan.bindings,
                &mut slots,
                name,
                Binding::data(Value::Undefined, true, true),
                true,
            );
            plan.functions.push((index, function));
        }
        if chunk.env_arguments {
            plan.arguments_slot = Some(slot(
                &mut plan.bindings,
                &mut slots,
                Rc::from("arguments"),
                Binding::data(Value::Undefined, true, true),
                true,
            ));
        }
        // ECMA-262 e28783d5 FunctionDeclarationInstantiation/GetBindingValue:
        // names are static here, while initialization, mutability and values stay
        // per invocation. The builder has already produced this exact ordered index.
        plan.layout = BindingLayout::fixed(chunk.binding_layout_id, slots);
        plan
    }

    pub(super) fn instantiate(
        &self,
        chunk: &Chunk,
        interp: &mut Interp,
        parent: &Env,
        this_value: &Value,
        arguments: &[Value],
    ) -> Env {
        let mut vars = VarMap::from_fixed_bindings(self.bindings.clone(), self.layout.as_ref());
        for &(slot, parameter) in &self.parameters {
            vars.initialize_fixed_value(
                slot,
                arguments
                    .get(usize::from(parameter))
                    .cloned()
                    .unwrap_or(Value::Undefined),
            );
        }
        if let Some(slot) = self.this_slot {
            vars.initialize_fixed_value(slot, this_value.clone());
        }
        let activation = new_var_scope_with_bindings(Some(parent.clone()), vars);
        {
            let mut scope = activation.borrow_mut();
            for name in &self.lexicals {
                scope.lexical_names.push(name.to_string());
            }
        }
        // Every closure captures this invocation's actual environment. The
        // plan contains no JS object, closure environment, or argument owner.
        for &(slot, function) in &self.functions {
            let value = interp.make_function(
                chunk.funcs[usize::from(function)].clone(),
                activation.clone(),
            );
            activation
                .borrow_mut()
                .vars
                .initialize_fixed_value(slot, value);
        }
        if let Some(slot) = self.arguments_slot {
            let value = Value::Obj(interp.make_compiled_arguments_object(arguments, &activation));
            activation
                .borrow_mut()
                .vars
                .initialize_fixed_value(slot, value);
        }
        activation
            .borrow_mut()
            .vars
            .publish_layout(chunk.binding_layout_id);
        activation
    }

    pub(super) fn scan_retained_memory(&self, visitor: &mut crate::memory::Visitor) -> usize {
        if let Some(layout) = &self.layout {
            visitor.binding_layout(layout);
        }
        for (name, _) in &self.bindings {
            visitor.rc_str(name);
        }
        for name in &self.lexicals {
            visitor.rc_str(name);
        }
        std::mem::size_of::<Self>()
            + self.bindings.capacity() * std::mem::size_of::<(Rc<str>, Binding)>()
            + (self.parameters.capacity() + self.functions.capacity())
                * std::mem::size_of::<(usize, u16)>()
            + self.lexicals.capacity() * std::mem::size_of::<Rc<str>>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bytecode::Tier, Completion, Engine};

    fn evaluate(engine: &mut Engine, source: &str) -> String {
        match engine
            .eval(source, false)
            .expect("activation fixture parses")
        {
            Completion::Value(value) => value,
            Completion::Throw { name, message } => panic!("{name}: {message}"),
        }
    }

    #[test]
    fn activation_plan_wide_closures_keep_fixed_slots_and_live_binding_guards() {
        for tier in [Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            assert_eq!(
                evaluate(
                    &mut engine,
                    r#"
                function make(seed) {
                    let a=seed,b=seed+1,c=seed+2,d=seed+3,e=seed+4,f=seed+5;
                    let g=seed+6,h=seed+7,j=seed+8,k=seed+9,l=seed+10,m=seed+11;
                    return [()=>a+b+c+d+e+f+g+h+j+k+l+m,()=>++a];
                }
                var left=make(1),right=make(10),one=left[0];
                for(let n=0;n<40;n++) { if(one()!==78 || right[0]()!==186) throw 'alias'; }
                left[1](); one()===79 && right[0]()===186
            "#
                ),
                "true"
            );
            let global = engine.interp.global_env.clone();
            let Value::Obj(one) = engine
                .interp
                .get_var("one", &global)
                .unwrap_or_else(|_| panic!("missing closure"))
            else {
                panic!("expected closure")
            };
            let activation = match &one.borrow().call {
                crate::value::Callable::User(user) => user.env.clone(),
                _ => panic!("expected user closure"),
            };
            if activation_plans_enabled() {
                let scope = activation.borrow();
                assert_ne!(scope.vars.layout_id(), 0);
                assert!(scope.vars.binding_slot("m").unwrap() >= 8);
            }
            // A dynamic insertion may replace the vector with a hash table.
            // Previously emitted slot/generation proofs must reject it.
            activation
                .borrow_mut()
                .vars
                .insert("added", Binding::data(Value::Num(7.), true, true));
            assert_eq!(activation.borrow().vars.layout_id(), 0);
            assert_eq!(evaluate(&mut engine, "one()"), "79");
            activation
                .borrow_mut()
                .vars
                .get_mut("m")
                .unwrap()
                .initialized = false;
            assert_eq!(
                evaluate(
                    &mut engine,
                    "try { one(); 'missed TDZ' } catch(e) { e.name }"
                ),
                "ReferenceError"
            );
            activation
                .borrow_mut()
                .vars
                .get_mut("m")
                .unwrap()
                .initialized = true;
            assert_eq!(evaluate(&mut engine, "one()"), "79");
        }
    }

    #[test]
    fn indexed_activation59_shares_wide_keys_but_keeps_values_and_mutations_private() {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            let declarations = (0..96)
                .map(|n| format!("let v{n}=seed+{n};"))
                .collect::<String>();
            let sum = (0..96)
                .map(|n| format!("v{n}"))
                .collect::<Vec<_>>()
                .join("+");
            let source = format!(
                "'use strict';function wide59(seed){{{declarations}return function(){{return {sum};}};}}
                 var left59=wide59(1),right59=wide59(2);
                 for(var n59=0;n59<30;n59++){{left59();right59();}}
                 left59()+'|'+right59();"
            );
            assert_eq!(evaluate(&mut engine, &source), "4656|4752", "{tier:?}");
            let environment = |engine: &mut Engine, name| {
                let global = engine.interp.global_env.clone();
                let value = engine
                    .interp
                    .get_var(name, &global)
                    .unwrap_or_else(|_| panic!("missing fixture closure {name}"));
                let object = value.as_obj().unwrap().borrow();
                let crate::value::Callable::User(user) = &object.call else {
                    panic!("fixture closure")
                };
                user.env.clone()
            };
            let left = environment(&mut engine, "left59");
            let right = environment(&mut engine, "right59");
            assert!(!Rc::ptr_eq(&left, &right));
            if tier != Tier::Interp
                && activation_plans_enabled()
                && crate::interpreter::indexed_activations_enabled()
            {
                let l = left.borrow();
                let r = right.borrow();
                assert!(Rc::ptr_eq(
                    l.vars.shared_layout().unwrap(),
                    r.vars.shared_layout().unwrap()
                ));
                assert!(l.vars.binding_slot("v95").is_some());
            }
            left.borrow_mut().vars.get_mut("v95").unwrap().value = Value::Num(1.);
            assert_eq!(evaluate(&mut engine, "left59()+'|'+right59()"), "4561|4752");
            left.borrow_mut()
                .vars
                .insert("added59", Binding::data(Value::Num(3.), true, true));
            assert_eq!(left.borrow().vars.layout_id(), 0);
            assert_eq!(evaluate(&mut engine, "left59()+'|'+right59()"), "4561|4752");
            right.borrow_mut().vars.get_mut("v95").unwrap().initialized = false;
            assert_eq!(
                evaluate(&mut engine, "try{right59();'miss'}catch(e){e.name}"),
                "ReferenceError"
            );
            right.borrow_mut().vars.get_mut("v95").unwrap().initialized = true;
            right.borrow_mut().vars.remove("v95");
            assert_eq!(right.borrow().vars.layout_id(), 0);
            assert_eq!(
                evaluate(&mut engine, "try{right59();'miss'}catch(e){e.name}"),
                "ReferenceError"
            );
            assert_eq!(evaluate(&mut engine, "left59()"), "4561");
            engine.interp.gc_collect();
        }
    }

    #[test]
    fn activation_plan_preserves_parameters_functions_this_and_arguments() {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            assert_eq!(
                evaluate(
                    &mut engine,
                    r#"
                function duplicate(a,a) { var a; return ()=>a; }
                function redeclare(a) { function a(){return 23;} return ()=>a(); }
                function lexical(x) {
                    var read=()=>x+y+z;
                    let y=x+2; const z=x+4;
                    return [read,()=>{try {z=99;}catch(e){return e.name;}}];
                }
                function receiver() {return ()=>this.n+arguments[0];}
                function early() {
                    var read=()=>late;
                    try {read();} catch(e) {return e.name;}
                    let late=1;
                }
                var a=lexical(2),b=lexical(20),f=receiver.call({n:7},9);
                [duplicate(1,8)(),redeclare(99)(),a[0](),b[0](),a[1](),f(),early()].join('|')
            "#
                ),
                "8|23|12|66|TypeError|16|ReferenceError",
                "{tier:?}"
            );
        }
    }

    #[test]
    fn activation_plan_does_not_replace_reused_or_eval_environments() {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            assert_eq!(
                evaluate(
                    &mut engine,
                    r#"
                function dynamic(a) {
                    var read=()=>a+injected;
                    eval('var injected=5');
                    arguments[0]=9;
                    return read();
                }
                function* generator(a) {
                    var read=()=>a;
                    arguments[0]=7; yield read();
                    a=11; yield arguments[0];
                }
                function defaults(a=()=>b) { var b=3; return a; }
                var it=generator(1),result=[dynamic(1),it.next().value,it.next().value];
                try { defaults()(); } catch(e) { result.push(e.name); }
                result.join('|')
            "#
                ),
                "14|7|11|ReferenceError",
                "{tier:?}"
            );
        }
    }
}
