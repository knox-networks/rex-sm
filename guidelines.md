# Guidelines

## State machine resource creation/allocation

* A state machine is created in three parts and in this sequence:
  1. `StateId`: creates a routable 'pointer' to the state machine
  2. `State`: creates the state enumerable (data that holds state transitions, starting with `Kind<State = OurState>::new_state()`)
  3. `storage::Tree`: commits the state enumerable and allows it to be updated

### Responsibility
* If a state machine has a parent, it is the **parent's responsibility to create the initial state and entry for the child state machine**
* A child state machine should **not** `StateMachineExt::create_tree` if a parent is involved
* A parent state machine is responsible for route creation in the child's `StateRouter`
* A state machine parent is responsible for all three steps in spawning a child state machine:

    ```rust
    // 1. create `StateId`
    let ping_id = StateId::new_rand(ComponentKind::Ping);
    let pong_id = StateId::new_rand(ComponentKind::Pong);
    // Menu + Ping + Pong
    // 2. create `State`
    let menu_tree = Node::new(id)
        .into_insert(Insert {
            parent_id: Some(id),
            id: ping_id,
        })
        .into_insert(Insert {
            parent_id: Some(id),
            id: pong_id,
        });

    // 3. create `storage::Tree`
    let tree = StateStore::new_tree(menu_tree);
    for id in [id, ping_id, pong_id] {
        ctx.state_store.insert_tree(id, tree.clone());
    }

    // signal to Ping state machine
    ctx.signal_queue.push_back(Signal {
        id: ping_id,
        input: Input::Ping(PingInput::StartSending(pong_id, who_sleeps)),
    });
    ```

## State machine input:
### 1. A state machine should do parsing/unwrapping in `<TestStateMachine as StateMachine>::process` before passing in a valid state and input
to a `TestStateMachine::process_inner`:

```rust
impl StateMachine<OuterKind, TestState, NotificationInput>
    for TestStateMachine<OuterKind>
{
    type Input = outer::Input;

    async fn process(
        &self,
        ctx: ProcessorContext<OuterKind>,
        id: StateId<OuterKind>,
        input: outer::Input,
    ) {
        let outer::Input::Ours(input) = input else {
            error!(?input, "expected outer::Input::Ours!");
            return;
        };

        // These are state creation inputs
        let state = self.get_state(&ctx, id).await;
        let Ok(state) = Self::parse_state_with_input(state, &input)
            .and_log(tracing::Level::INFO) else {
            return;
        };

        self
            .process_inner(ctx.clone(), id, input, state)
            .await
            .log_err():
    }

    fn get_kind(&self) -> OuterKind {
        OuterKind::Ours
    }
}
```

### 2. A state machine should implement a `Self::parse_state_with_input`

`Self::parse_state_with_input` is meant to parse an `outer::Input` into an
`inner::Input`

`Self::parse_state_with_input` should include ensure these outcomes:
1. `Self::invalid_state(state)` produces an `Err`
2. `(Option<outer::State>::None, inner::Input::New(...))` should create a new
   state through  `State::default()`
3. An input that is **not** meant to create a state should return an `Err`
   if `self.get_state(...).is_none()`
4. A valid `outer::State` return an `State` _even if_ the input variant
   is invalid for the current state: this complexity should be reserved for
   `process_inner`
5. any state that is not `outer::State::Ours` should panic or return an error
```rust
impl StateMachine<OuterKind, TestState, NotificationInput>
    for TestStateMachine<OuterKind>
{
    type Input = outer::Input;

    async fn process(
        &self,
        ctx: ProcessorContext<OuterKind>,
        id: StateId<OuterKind>,
        input: outer::Input,
    ) {
        // ...

        // These are state creation inputs
        let state = self.get_state(&ctx, id).await;
        let Ok(state) = Self::parse_state_with_input(state, &input)
            .and_log(tracing::Level::INFO) else {
            return;
        };

        // ...
    }

    // Match current state with current input
    fn parse_state_with_input(
        state: Option<outer::State>,
        input: &inner::Input,
    ) -> Result<our::State, Report<ContractError>> {
        match (state, input) {
            // 1.
            (Some(s), _) if Self::invalid_state(s) => {
                Err(InvalidStatus::with_kv_dbg("state", s).change_context(ContractError))
            }
            // 2.
            (None, Input::New(_, _)) => Ok(State::default()),
            // 3.
            (None, _) => Err(InvalidInput::attach("contract::State does not exist")
                .change_context(ContractError)),
            // 4.
            (Some(outer::State::Ours(s)), _) => Ok(s),
            // 5.
            (Some(s), _) => {
                panic!("Invalid state stored: {s:?}, this is a bug");
            }
        }
    }

### 3. A state machine should not have an input that is _exclusive_ to parent signals:
```rust
impl StateMachine<OuterKind, TestState, NotificationInput>
    for TestStateMachine<OuterKind>
{
    type Input = outer::Input;

    // fn process (...);
    // fn get_kind(&self) -> OuterKind;
    todo!();
}

impl TestStateMachine<OuterKind> {
    async fn process_inner(
        &self,
        ctx: TestContext,
        id: StateId<OuterKind>,
        input: Input,
        state: State,
    ) -> Result<(), Error> {
        match input {
            // this input is for creating `storage::Tree`
            // for a state machine _without_ a parent
            Input::New(_field_only_used_by_router, data) => {
                self.create_tree(&ctx, id);
                // ◊ this creation logic is shared with `Input::SendData`
                // if the input was generated from a parent state machine
                // signal
                // NOTE: this does _not_ create state data
                self.insert_data(id, data);
                // † this is where data is sent
                self.notify(&ctx, ack_data(data));
            }
            Input::SendData(some_data) => {
                // this input is sent from a parent state machine
                // and thus these things _already_ exist:
                // 1. `StateId`
                // 2. `State`
                // 3. `storage::Tree`
                if let Some(data) = some_data {
                    // ◊ this creation logic is shared with `Input::New`
                    self.insert_data(id, data.clone());
                    // † this is where data is sent
                    self.notify(&ctx, ack_data(data));
                } else {
                    let data = self.get_data(id).unwrap_or_else(|| NotFound::attach_kv("id", id))?;
                    // † this is where data is sent
                    self.notify(&ctx, ack_data(data));
                }
            }
            _ => todo!(),
        }
        Ok(())
    }
}
```

### 4. A state machine should spend as little time as possible emitting/processing `Notifications` that do not impact its own state transitions

```rust
impl TestStateMachine<OuterKind> {
    async fn process_inner(
        &self,
        ctx: TestContext,
        id: StateId<OuterKind>,
        input: Input,
        state: State,
    ) -> Result<(), Error> {
        match input {
            Input::New(data, metadata) => {
                self.create_tree(&ctx, id);
                // ◊ this creation logic is shared with `Input::SendData`
                // if the input was generated from a parent state machine
                // signal
                // NOTE: this does _not_ create state data
                self.insert_data(id, data);
                // expensive operation converting bytes to complex data structures
                // this notification is used to emit events
                let parsed_metadata = metadata.parse()?;
                self.notify(&ctx, parsed_metadata));
            }
            _ => todo!(),
        }
        Ok(())
    }
}
```

In the example above, `parsed_metadata` should be processed downstream _after_ `Input` is processed
by `TestStateMachine` to unblock the hot path of state machine input processing

### 5. No `Input` should be processed by a state that is failed or completed
<!-- TODO add details -->
* completed and failed states persist as a matter of convenience for any input producers
* Logic that depends on failed states will behave nondetermenistically because they depend upon `StateStore` _not_ doing garbage collection on finished states

The proper location for this type of logic should depend on data stored  as a result of a `Notification` produced from a failed or completed state


### 6. State failures and completions should be explicit

The example below is problematic because it creates a blanket failure scenario where _any_ error emitted from
`Self::process_inner` will trigger a failed state:
```rust
impl TestStateMachine<TxnKind, TxnState, NotificationInput>
    for ConditionalTxnStateMachine<TxnKind>
{
    type Input = transaction::Input;

    async fn process(
        &self,
        ctx: ProcessorContext<TxnKind>,
        id: StateId<TxnKind>,
        input: Self::Input,
    ) {
        // ...

        if self
            .process_inner(&ctx, id, input, state)
            .await
            .and_log_err()
            .is_err()
        {
            self.fail(&ctx, id).await;
        }
    }
}
```

`Input` producers should be able to re-emit inputs that are invalid or unparsable and should not resort to attempting to produce a new state machine due to intolerance of fault.

### 7. Request/Response pairs should use imperative and part participle verbiage respectively

The example naming conversions to use when creating `Notification` commands,
as well as their `Signal` response analogues:

* Requests should use [imperative mood](https://en.wikipedia.org/wiki/Imperative_mood) verbiage.
  _Ex:_ `ConfirmOrder`
* Responses should use [past-participle](https://en.wikipedia.org/wiki/Participle#Forms) verbiage.
  _Ex:_ `OrderConfirmed`

```rust
pub enum StorageRequest {
    StoreId(StateId<TxnKind>, Uuid),
    StoreSecret(StateId<TxnKind>, String),
}

pub enum StorageResponse {
    IdStored(Result<(), ()>),
    SecretStored(Result<(), ()>),
}

impl From<StorageResponse> for TestStateMachineInput {
    fn from(val: StorageResponse) -> Self {
        unimplemented!()
    }

}

pub enum TestStateMachineInput {
    IdStored(Result<(), ()>),
    SecretStored(Result<(), ()>),
}
```
