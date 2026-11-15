//! The flow engine on Temporal.
//!
//! A pipeline is not a workflow written for it. There is one workflow, the
//! engine, and what it is given to run is a flow definition: the same TOML
//! graph of steps and routes the factory already keeps in its config. The
//! definition travels in the workflow's input, so a new pipeline or a changed
//! one is data and needs no new code and no versioning. Only a change to the
//! engine itself is a change to workflow code.
//!
//! Every step is an activity, and what an activity does is not the engine's
//! business: see [`steps::StepRunner`].

pub mod accounts;
pub mod engine;
pub mod factory;
pub mod steps;
pub mod worker;
