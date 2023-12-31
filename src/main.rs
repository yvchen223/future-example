use std::{
    future::Future, pin::Pin, sync::{ mpsc::{channel, Sender}, Arc, Mutex,},
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker}, mem,
    thread::{self, JoinHandle}, time::{Duration, Instant}, collections::HashMap
};
use std::sync::Condvar;

fn main() {
    let start = Instant::now();
    let reactor = Reactor::new();

    let future1 = Task::new(reactor.clone(), 1, 1);
    let future2 = Task::new(reactor.clone(), 2, 2);

    let fut1 = async {
        let val = future1.await;
        println!("Got {} at time: {:2}.", val, start.elapsed().as_secs_f32());
    };
    let fut2 = async {
        let val = future2.await;
        println!("Got {} at time: {:2}.", val, start.elapsed().as_secs_f32());
    };

    let mainfut = async {
        fut1.await;
        fut2.await;
    };
    block_on(mainfut);
}

fn block_on<F: Future>(mut future: F) -> F::Output {
    let parker = Arc::new(Parker::default());
    let mywaker = Arc::new(MyWaker { parker: parker.clone() });
    let waker = mywaker_into_waker(Arc::into_raw(mywaker));

    let mut cx = Context::from_waker(&waker);

    let mut future = unsafe { Pin::new_unchecked(&mut future) };

    let val = loop {
        match Future::poll(future.as_mut(), &mut cx) {
            Poll::Ready(val) => break val,
            Poll::Pending => parker.park(),
        };
    };
    val
}

#[derive(Default)]
struct Parker(Mutex<bool>, Condvar);

impl Parker {
    fn park(&self) {
        let mut resumable = self.0.lock().unwrap();
        while !*resumable {
            resumable = self.1.wait(resumable).unwrap();
        }
        *resumable = false
    }

    fn unpark(&self) {
        *self.0.lock().unwrap() = true;
        self.1.notify_one();
    }
}

/// The definition of our `Waker`.
#[derive(Clone)]
struct MyWaker {
    parker: Arc<Parker>
}

/// The definition of our `Future`. It keeps all the information we need.
#[derive(Clone)]
struct Task {
    id: usize,
    reactor: Arc<Mutex<Box<Reactor>>>,
    data: u64,
}

fn mywaker_wake(s: &MyWaker) {
    let waker_ptr: *const MyWaker = s;
    let waker_arc = unsafe { Arc::from_raw(waker_ptr) };
    waker_arc.parker.unpark();
}

fn mywaker_clone(s: &MyWaker) -> RawWaker {
    let arc = unsafe { Arc::from_raw(s) };
    std::mem::forget(arc.clone());
    RawWaker::new(Arc::into_raw(arc) as *const (), &VTABLE)
}

const VTABLE: RawWakerVTable = unsafe {
    RawWakerVTable::new(
        |s| mywaker_clone(&*(s as *const MyWaker)),  // clone
        |s| mywaker_wake(&*(s as *const MyWaker)),  // wake
        |s| (*(s as *const MyWaker)).parker.unpark(),  // wake by ref
        |s| drop(Arc::from_raw(s as *const MyWaker)),  // decrease refcount
    )
};

fn mywaker_into_waker(s: *const MyWaker) -> Waker {
    let raw_waker = RawWaker::new(s as *const (), &VTABLE);
    unsafe { Waker::from_raw(raw_waker) }
}

impl Task {
    fn new(reactor: Arc<Mutex<Box<Reactor>>>, data: u64, id: usize) -> Self {
        Task { id, reactor, data }
    }
}

impl Future for Task {
    type Output = usize;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut r = self.reactor.lock().unwrap();

        // First we check if the task is marked as ready.
        if r.is_ready(self.id) {
            // If it's ready we set its state to `Finished`.
            *r.tasks.get_mut(&self.id).unwrap() = TaskState::Finished;
            Poll::Ready(self.id)
        // If it isn't finished we check the map we have stored in our `Reactor`.
        } else if r.tasks.contains_key(&self.id) {
            r.tasks.insert(self.id, TaskState::NotReady(cx.waker().clone()));
            Poll::Pending
        } else {
            r.register(self.data, cx.waker().clone(), self.id);
            Poll::Pending
        }
    }
}

enum TaskState {
    Ready,
    NotReady(Waker),
    Finished,
}

struct Reactor {
    // We need some way of registering a Task with the reactor.
    dispatcher: Sender<Event>,
    handle: Option<JoinHandle<()>>,
    // A list of tasks.
    tasks: HashMap<usize, TaskState>,
}

// This represents the Events we can send to our reactor thread.
#[derive(Debug)]
enum Event {
    Close,
    Timeout(u64, usize),
}

impl Reactor {
    fn new() -> Arc<Mutex<Box<Self>>> {
        let (tx, rx) = channel::<Event>();
        let reactor = Arc::new(Mutex::new(Box::new(Reactor {
            dispatcher: tx,
            handle: None,
            tasks: HashMap::new(),
        })));

        let reactor_clone = Arc::downgrade(&reactor);

        // This will be our Reactor-thread. It will spawn new threads which will serve as timers.
        let handle = thread::spawn(move || {
            let mut handles = vec![];
            for event in rx {
                println!("REACTOR: {:?}", event);
                let reactor = reactor_clone.clone();
                match event {
                    Event::Close => break,
                    Event::Timeout(duration, id) => {
                        // Spawn a new thread that will serve as a timer and will call `wake`.
                        let event_handle = thread::spawn(move || {
                            thread::sleep(Duration::from_secs(duration));
                            let reactor = reactor.upgrade().unwrap();
                            reactor.lock().map(|mut r| r.wake(id)).unwrap();
                        });
                        handles.push(event_handle);
                    }
                }
            }
            // This is important for us since we need to know that these threads don't live longer than our
            // Reactor-thread.
            handles.into_iter().for_each(|handle| handle.join().unwrap());
        });
        reactor.lock().map(|mut r| r.handle = Some(handle)).unwrap();
        reactor
    }

    fn wake(&mut self, id: usize) {
        self.tasks.get_mut(&id).map(|state| {
            match mem::replace(state, TaskState::Ready) {
                TaskState::NotReady(waker) => waker.wake(),
                TaskState::Finished => panic!("Called 'wake' twice on task: {}", id),
                _ => unreachable!(),
            }
        }).unwrap();
    }

    fn register(&mut self, duration: u64, waker: Waker, id: usize) {
        if self.tasks.insert(id, TaskState::NotReady(waker)).is_some() {
            panic!("Register a task twice with id: {}", id);
        }
        self.dispatcher.send(Event::Timeout(duration, id)).unwrap();
    }

    fn is_ready(&self, id: usize) -> bool {
        self.tasks.get(&id).map(|state| match state {
            TaskState::Ready => true,
            _ => false,
        }).unwrap_or(false)
    }
}

impl Drop for Reactor {
    fn drop(&mut self) {
        self.dispatcher.send(Event::Close).unwrap();
        self.handle.take().map(|h| h.join().unwrap()).unwrap()
    }
}