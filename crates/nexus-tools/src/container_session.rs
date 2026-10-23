use std::sync::Mutex;

pub(crate) struct ContainerSession<T> {
    pub(crate) inner: Mutex<SessionState<T>>,
}

pub(crate) struct SessionState<T> {
    pub(crate) transport: T,
    pub(crate) poisoned: bool,
}

impl<T> ContainerSession<T> {
    pub(crate) fn new(transport: T) -> Self {
        Self {
            inner: Mutex::new(SessionState {
                transport,
                poisoned: false,
            }),
        }
    }
}
