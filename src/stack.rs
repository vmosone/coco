//! A lock-free stack.
//!
//! This is an implementation of the Treiber stack, one of the simplest lock-free data structures.

use std::ptr;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed};

use epoch::{self, Atomic, Owned};

/// A single node in a stack.
struct Node<T> {
    /// The payload.
    value: T,
    /// The next node in the stack.
    next: Atomic<Node<T>>,
}

/// A lock-free stack.
///
/// It can be used with multiple producers and multiple consumers at the same time.
pub struct Stack<T> {
    head: Atomic<Node<T>>,
}

unsafe impl<T: Send> Send for Stack<T> {}
unsafe impl<T: Send> Sync for Stack<T> {}

impl<T> Stack<T> {
    /// Returns a new, empty stack.
    ///
    /// # Examples
    ///
    /// ```
    /// use coco::Stack;
    ///
    /// let s = Stack::<i32>::new();
    /// ```
    pub fn new() -> Self {
        Stack { head: Atomic::null() }
    }

    /// Returns `true` if the stack is empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use coco::Stack;
    ///
    /// let s = Stack::new();
    /// assert!(s.is_empty());
    /// s.push("hello");
    /// assert!(!s.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        epoch::pin(|scope| self.head.load(Acquire, scope).is_null())
    }

    /// Pushes a new value onto the stack.
    ///
    /// # Examples
    ///
    /// ```
    /// use coco::Stack;
    ///
    /// let s = Stack::new();
    /// s.push(1);
    /// s.push(2);
    /// ```
    pub fn push(&self, value: T) {
        let mut node = Owned::new(Node {
            value: value,
            next: Atomic::null(),
        });

        epoch::pin(|scope| {
            let mut head = self.head.load(Acquire, scope);
            loop {
                node.next.store(head, Relaxed);
                match self.head.compare_and_swap_weak_owned(head, node, AcqRel, scope) {
                    Ok(_) => break,
                    Err((h, n)) => {
                        head = h;
                        node = n;
                    }
                }
            }
        })
    }

    /// Attempts to pop an value from the stack.
    ///
    /// Returns `None` if the stack is empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use coco::Stack;
    ///
    /// let s = Stack::new();
    /// s.push(1);
    /// s.push(2);
    /// assert_eq!(s.pop(), Some(2));
    /// assert_eq!(s.pop(), Some(1));
    /// assert_eq!(s.pop(), None);
    /// ```
    pub fn pop(&self) -> Option<T> {
        epoch::pin(|scope| {
            let mut head = self.head.load(Acquire, scope);
            loop {
                match unsafe { head.as_ref() } {
                    Some(h) => {
                        let next = h.next.load(Acquire, scope);
                        match self.head.compare_and_swap_weak(head, next, AcqRel, scope) {
                            Ok(()) => unsafe {
                                scope.defer_free(head);
                                return Some(ptr::read(&h.value));
                            },
                            Err(h) => head = h,
                        }
                    }
                    None => return None,
                }
            }
        })
    }
}

impl<T> Drop for Stack<T> {
    fn drop(&mut self) {
        // Destruct all nodes in the stack.
        unsafe {
            epoch::unprotected(|scope| {
                let mut curr = self.head.load(Relaxed, scope).as_raw();
                while !curr.is_null() {
                    let next = (*curr).next.load(Relaxed, scope).as_raw();
                    drop(Box::from_raw(curr as *mut Node<T>));
                    curr = next;
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate rand;

    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering::SeqCst;
    use std::thread;

    use super::Stack;
    use self::rand::Rng;

    #[test]
    fn smoke() {
        let s = Stack::new();
        s.push(1);
        assert_eq!(s.pop(), Some(1));
        assert_eq!(s.pop(), None);
    }

    #[test]
    fn push_pop() {
        let s = Stack::new();
        s.push(1);
        s.push(2);
        s.push(3);
        assert_eq!(s.pop(), Some(3));
        s.push(4);
        assert_eq!(s.pop(), Some(4));
        assert_eq!(s.pop(), Some(2));
        assert_eq!(s.pop(), Some(1));
        assert_eq!(s.pop(), None);
        s.push(5);
        assert_eq!(s.pop(), Some(5));
        assert_eq!(s.pop(), None);
    }

    #[test]
    fn is_empty() {
        let s = Stack::new();
        assert!(s.is_empty());

        for i in 0..3 {
            s.push(i);
            assert!(!s.is_empty());
        }

        for _ in 0..3 {
            assert!(!s.is_empty());
            s.pop();
        }

        assert!(s.is_empty());
        s.push(3);
        assert!(!s.is_empty());
        s.pop();
        assert!(s.is_empty());
    }

    #[test]
    fn stress() {
        const THREADS: usize = 8;

        let s = Arc::new(Stack::new());
        let len = Arc::new(AtomicUsize::new(0));

        let threads = (0..THREADS).map(|t| {
            let s = s.clone();
            let len = len.clone();

            thread::spawn(move || {
                let mut rng = rand::thread_rng();
                for i in 0..100_000 {
                    if rng.gen_range(0, t + 1) == 0 {
                        if s.pop().is_some() {
                            len.fetch_sub(1, SeqCst);
                        }
                    } else {
                        s.push(t + THREADS * i);
                        len.fetch_add(1, SeqCst);
                    }
                }
            })
        }).collect::<Vec<_>>();

        for t in threads {
            t.join().unwrap();
        }

        let mut last = [::std::usize::MAX; THREADS];

        while !s.is_empty() {
            let x = s.pop().unwrap();
            let t = x % THREADS;

            assert!(last[t] > x);
            last[t] = x;

            len.fetch_sub(1, SeqCst);
        }
        assert_eq!(len.load(SeqCst), 0);
    }

    #[test]
    fn destructors() {
        struct Elem((), Arc<AtomicUsize>);

        impl Drop for Elem {
            fn drop(&mut self) {
                self.1.fetch_add(1, SeqCst);
            }
        }

        const THREADS: usize = 8;

        let s = Arc::new(Stack::new());
        let len = Arc::new(AtomicUsize::new(0));
        let popped = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));

        let threads = (0..THREADS).map(|t| {
            let s = s.clone();
            let len = len.clone();
            let popped = popped.clone();
            let dropped = dropped.clone();

            thread::spawn(move || {
                let mut rng = rand::thread_rng();
                for _ in 0..100_000 {
                    if rng.gen_range(0, t + 1) == 0 {
                        if s.pop().is_some() {
                            len.fetch_sub(1, SeqCst);
                            popped.fetch_add(1, SeqCst);
                        }
                    } else {
                        s.push(Elem((), dropped.clone()));
                        len.fetch_add(1, SeqCst);
                    }
                }
            })
        }).collect::<Vec<_>>();

        for t in threads {
            t.join().unwrap();
        }

        assert_eq!(dropped.load(SeqCst), popped.load(SeqCst));
        drop(s);
        assert_eq!(dropped.load(SeqCst), popped.load(SeqCst) + len.load(SeqCst));
    }
}
