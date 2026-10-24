use std::sync::{Arc, Mutex};

pub(crate) struct ContainerSession<T> {
    pub(crate) inner: Mutex<SessionState<T>>,
}

pub(crate) struct SessionState<T> {
    pub(crate) transport: T,
    pub(crate) poisoned: bool,
    poison_handler: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl<T> SessionState<T> {
    pub(crate) fn poison(&mut self) {
        if self.poisoned {
            return;
        }
        self.poisoned = true;
        if let Some(handler) = &self.poison_handler {
            handler();
        }
    }
}

impl<T> ContainerSession<T> {
    pub(crate) fn new(transport: T) -> Self {
        Self::with_poison_handler(transport, None)
    }

    pub(crate) fn with_poison_handler(
        transport: T,
        poison_handler: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Self {
        Self {
            inner: Mutex::new(SessionState {
                transport,
                poisoned: false,
                poison_handler,
            }),
        }
    }
}
