/*! Runtime support for message passing with protocol enforcement.


Pipes consist of two endpoints. One endpoint can send messages and
the other can receive messages. The set of legal messages and which
directions they can flow at any given point are determined by a
protocol. Below is an example protocol.

~~~
proto! pingpong {
    ping: send {
        ping -> pong
    }
    pong: recv {
        pong -> ping
    }
}
~~~

The `proto!` syntax extension will convert this into a module called
`pingpong`, which includes a set of types and functions that can be
used to write programs that follow the pingpong protocol.

*/

/* IMPLEMENTATION NOTES

The initial design for this feature is available at:

https://github.com/eholk/rust/wiki/Proposal-for-channel-contracts

Much of the design in that document is still accurate. There are
several components for the pipe implementation. First of all is the
syntax extension. To see how that works, it is best see comments in
libsyntax/ext/pipes.rs.

This module includes two related pieces of the runtime
implementation. There is support for unbounded and bounded
protocols. The main difference between the two is the type of the
buffer that is carried along in the endpoint data structures.

FIXME (#3072) - This is still incomplete

*/

import unsafe::{forget, reinterpret_cast, transmute};
import either::{either, left, right};
import option::unwrap;
import arc::methods;

// Things used by code generated by the pipe compiler.
export entangle, get_buffer, drop_buffer;
export send_packet_buffered, recv_packet_buffered;
export packet, mk_packet, entangle_buffer, has_buffer, buffer_header;

// export these so we can find them in the buffer_resource
// destructor. This is probably a symptom of #3005.
export atomic_add_acq, atomic_sub_rel;

// User-level things
export send_packet, recv_packet, send, recv, try_recv, peek;
export select, select2, selecti, select2i, selectable;
export spawn_service, spawn_service_recv;
export stream, port, chan, shared_chan, port_set, channel;

#[doc(hidden)]
const SPIN_COUNT: uint = 0;

macro_rules! move_it {
    { $x:expr } => { unsafe { let y <- *ptr::addr_of($x); y } }
}

#[doc(hidden)]
enum state {
    empty,
    full,
    blocked,
    terminated
}

struct buffer_header {
    // Tracks whether this buffer needs to be freed. We can probably
    // get away with restricting it to 0 or 1, if we're careful.
    let mut ref_count: int;

    new() { self.ref_count = 0; }

    // We may want a drop, and to be careful about stringing this
    // thing along.
}

// This is for protocols to associate extra data to thread around.
#[doc(hidden)]
type buffer<T: send> = {
    header: buffer_header,
    data: T,
};

struct packet_header {
    let mut state: state;
    let mut blocked_task: option<*rust_task>;

    // This is a reinterpret_cast of a ~buffer, that can also be cast
    // to a buffer_header if need be.
    let mut buffer: *libc::c_void;

    new() {
        self.state = empty;
        self.blocked_task = none;
        self.buffer = ptr::null();
    }

    // Returns the old state.
    unsafe fn mark_blocked(this: *rust_task) -> state {
        self.blocked_task = some(this);
        swap_state_acq(self.state, blocked)
    }

    unsafe fn unblock() {
        assert self.state != blocked || self.blocked_task != none;
        self.blocked_task = none;
        alt swap_state_acq(self.state, empty) {
          empty | blocked { }
          terminated { self.state = terminated; }
          full { self.state = full; }
        }
    }

    // unsafe because this can do weird things to the space/time
    // continuum. It ends making multiple unique pointers to the same
    // thing. You'll proobably want to forget them when you're done.
    unsafe fn buf_header() -> ~buffer_header {
        assert self.buffer.is_not_null();
        reinterpret_cast(self.buffer)
    }

    fn set_buffer<T: send>(b: ~buffer<T>) unsafe {
        self.buffer = reinterpret_cast(b);
    }
}

#[doc(hidden)]
type packet<T: send> = {
    header: packet_header,
    mut payload: option<T>,
};

trait has_buffer {
    fn set_buffer(b: *libc::c_void);
}

impl methods<T: send> of has_buffer for packet<T> {
    fn set_buffer(b: *libc::c_void) {
        self.header.buffer = b;
    }
}

fn mk_packet<T: send>() -> packet<T> {
    {
        header: packet_header(),
        mut payload: none
    }
}

fn unibuffer<T: send>() -> ~buffer<packet<T>> {
    let b = ~{
        header: buffer_header(),
        data: {
            header: packet_header(),
            mut payload: none,
        }
    };

    unsafe {
        b.data.header.buffer = reinterpret_cast(b);
    }

    b
}

#[doc(hidden)]
fn packet<T: send>() -> *packet<T> {
    let b = unibuffer();
    let p = ptr::addr_of(b.data);
    // We'll take over memory management from here.
    unsafe { forget(b) }
    p
}

#[doc(hidden)]
fn entangle_buffer<T: send, Tstart: send>(
    -buffer: ~buffer<T>,
    init: fn(*libc::c_void, x: &T) -> *packet<Tstart>)
    -> (send_packet_buffered<Tstart, T>, recv_packet_buffered<Tstart, T>)
{
    let p = init(unsafe { reinterpret_cast(buffer) }, &buffer.data);
    unsafe { forget(buffer) }
    (send_packet_buffered(p), recv_packet_buffered(p))
}

#[abi = "rust-intrinsic"]
extern mod rusti {
    fn atomic_xchng(&dst: int, src: int) -> int;
    fn atomic_xchng_acq(&dst: int, src: int) -> int;
    fn atomic_xchng_rel(&dst: int, src: int) -> int;

    fn atomic_add_acq(&dst: int, src: int) -> int;
    fn atomic_sub_rel(&dst: int, src: int) -> int;
}

// If I call the rusti versions directly from a polymorphic function,
// I get link errors. This is a bug that needs investigated more.
#[doc(hidden)]
fn atomic_xchng_rel(&dst: int, src: int) -> int {
    rusti::atomic_xchng_rel(dst, src)
}

#[doc(hidden)]
fn atomic_add_acq(&dst: int, src: int) -> int {
    rusti::atomic_add_acq(dst, src)
}

#[doc(hidden)]
fn atomic_sub_rel(&dst: int, src: int) -> int {
    rusti::atomic_sub_rel(dst, src)
}

#[doc(hidden)]
type rust_task = libc::c_void;

extern mod rustrt {
    #[rust_stack]
    fn rust_get_task() -> *rust_task;

    #[rust_stack]
    fn task_clear_event_reject(task: *rust_task);

    fn task_wait_event(this: *rust_task, killed: &mut *libc::c_void) -> bool;
    pure fn task_signal_event(target: *rust_task, event: *libc::c_void);
}

#[doc(hidden)]
fn wait_event(this: *rust_task) -> *libc::c_void {
    let mut event = ptr::null();

    let killed = rustrt::task_wait_event(this, &mut event);
    if killed && !task::failing() {
        fail ~"killed"
    }
    event
}

#[doc(hidden)]
fn swap_state_acq(&dst: state, src: state) -> state {
    unsafe {
        reinterpret_cast(rusti::atomic_xchng_acq(
            *(ptr::mut_addr_of(dst) as *mut int),
            src as int))
    }
}

#[doc(hidden)]
fn swap_state_rel(&dst: state, src: state) -> state {
    unsafe {
        reinterpret_cast(rusti::atomic_xchng_rel(
            *(ptr::mut_addr_of(dst) as *mut int),
            src as int))
    }
}

#[doc(hidden)]
unsafe fn get_buffer<T: send>(p: *packet_header) -> ~buffer<T> {
    transmute((*p).buf_header())
}

struct buffer_resource<T: send> {
    let buffer: ~buffer<T>;
    new(+b: ~buffer<T>) {
        //let p = ptr::addr_of(*b);
        //error!{"take %?", p};
        atomic_add_acq(b.header.ref_count, 1);
        self.buffer = b;
    }

    drop unsafe {
        let b = move_it!{self.buffer};
        //let p = ptr::addr_of(*b);
        //error!{"drop %?", p};
        let old_count = atomic_sub_rel(b.header.ref_count, 1);
        //let old_count = atomic_xchng_rel(b.header.ref_count, 0);
        if old_count == 1 {
            // The new count is 0.

            // go go gadget drop glue
        }
        else {
            forget(b)
        }
    }
}

#[doc(hidden)]
fn send<T: send, Tbuffer: send>(-p: send_packet_buffered<T, Tbuffer>,
                                -payload: T) {
    let header = p.header();
    let p_ = p.unwrap();
    let p = unsafe { &*p_ };
    assert ptr::addr_of(p.header) == header;
    assert p.payload == none;
    p.payload <- some(payload);
    let old_state = swap_state_rel(p.header.state, full);
    alt old_state {
      empty {
        // Yay, fastpath.

        // The receiver will eventually clean this up.
        //unsafe { forget(p); }
      }
      full { fail ~"duplicate send" }
      blocked {
        debug!{"waking up task for %?", p_};
        alt p.header.blocked_task {
          some(task) {
            rustrt::task_signal_event(
                task, ptr::addr_of(p.header) as *libc::c_void);
          }
          none { debug!{"just kidding!"} }
        }

        // The receiver will eventually clean this up.
        //unsafe { forget(p); }
      }
      terminated {
        // The receiver will never receive this. Rely on drop_glue
        // to clean everything up.
      }
    }
}

/** Receives a message from a pipe.

Fails if the sender closes the connection.

*/
fn recv<T: send, Tbuffer: send>(-p: recv_packet_buffered<T, Tbuffer>) -> T {
    option::unwrap(try_recv(p))
}

/** Attempts to receive a message from a pipe.

Returns `none` if the sender has closed the connection without sending
a message, or `some(T)` if a message was received.

*/
fn try_recv<T: send, Tbuffer: send>(-p: recv_packet_buffered<T, Tbuffer>)
    -> option<T>
{
    let p_ = p.unwrap();
    let p = unsafe { &*p_ };
    let this = rustrt::rust_get_task();
    rustrt::task_clear_event_reject(this);
    p.header.blocked_task = some(this);
    let mut first = true;
    let mut count = SPIN_COUNT;
    loop {
        rustrt::task_clear_event_reject(this);
        let old_state = swap_state_acq(p.header.state,
                                       blocked);
        alt old_state {
          empty {
            debug!{"no data available on %?, going to sleep.", p_};
            if count == 0 {
                wait_event(this);
            }
            else {
                count -= 1;
                // FIXME (#524): Putting the yield here destroys a lot
                // of the benefit of spinning, since we still go into
                // the scheduler at every iteration. However, without
                // this everything spins too much because we end up
                // sometimes blocking the thing we are waiting on.
                task::yield();
            }
            debug!{"woke up, p.state = %?", copy p.header.state};
          }
          blocked {
            if first {
                fail ~"blocking on already blocked packet"
            }
          }
          full {
            let mut payload = none;
            payload <-> p.payload;
            p.header.state = empty;
            return some(option::unwrap(payload))
          }
          terminated {
            assert old_state == terminated;
            return none;
          }
        }
        first = false;
    }
}

/// Returns true if messages are available.
pure fn peek<T: send, Tb: send>(p: recv_packet_buffered<T, Tb>) -> bool {
    alt unsafe {(*p.header()).state} {
      empty { false }
      blocked { fail ~"peeking on blocked packet" }
      full | terminated { true }
    }
}

impl peek<T: send, Tb: send> for recv_packet_buffered<T, Tb> {
    pure fn peek() -> bool {
        peek(self)
    }
}

#[doc(hidden)]
fn sender_terminate<T: send>(p: *packet<T>) {
    let p = unsafe { &*p };
    alt swap_state_rel(p.header.state, terminated) {
      empty {
        // The receiver will eventually clean up.
        //unsafe { forget(p) }
      }
      blocked {
        // wake up the target
        let target = p.header.blocked_task.get();
        rustrt::task_signal_event(target,
                                  ptr::addr_of(p.header) as *libc::c_void);

        // The receiver will eventually clean up.
        //unsafe { forget(p) }
      }
      full {
        // This is impossible
        fail ~"you dun goofed"
      }
      terminated {
        // I have to clean up, use drop_glue
      }
    }
}

#[doc(hidden)]
fn receiver_terminate<T: send>(p: *packet<T>) {
    let p = unsafe { &*p };
    alt swap_state_rel(p.header.state, terminated) {
      empty {
        // the sender will clean up
        //unsafe { forget(p) }
      }
      blocked {
        // this shouldn't happen.
        fail ~"terminating a blocked packet"
      }
      terminated | full {
        // I have to clean up, use drop_glue
      }
    }
}

/** Returns when one of the packet headers reports data is available.

This function is primarily intended for building higher level waiting
functions, such as `select`, `select2`, etc.

It takes a vector slice of packet_headers and returns an index into
that vector. The index points to an endpoint that has either been
closed by the sender or has a message waiting to be received.

*/
fn wait_many(pkts: &[*packet_header]) -> uint {
    let this = rustrt::rust_get_task();

    rustrt::task_clear_event_reject(this);
    let mut data_avail = false;
    let mut ready_packet = pkts.len();
    for pkts.eachi |i, p| unsafe {
        let p = unsafe { &*p };
        let old = p.mark_blocked(this);
        alt old {
          full | terminated {
            data_avail = true;
            ready_packet = i;
            (*p).state = old;
            break;
          }
          blocked { fail ~"blocking on blocked packet" }
          empty { }
        }
    }

    while !data_avail {
        debug!{"sleeping on %? packets", pkts.len()};
        let event = wait_event(this) as *packet_header;
        let pos = vec::position(pkts, |p| p == event);

        alt pos {
          some(i) {
            ready_packet = i;
            data_avail = true;
          }
          none {
            debug!{"ignoring spurious event, %?", event};
          }
        }
    }

    debug!{"%?", pkts[ready_packet]};

    for pkts.each |p| { unsafe{ (*p).unblock()} }

    debug!{"%?, %?", ready_packet, pkts[ready_packet]};

    unsafe {
        assert (*pkts[ready_packet]).state == full
            || (*pkts[ready_packet]).state == terminated;
    }

    ready_packet
}

/** Receives a message from one of two endpoints.

The return value is `left` if the first endpoint received something,
or `right` if the second endpoint receives something. In each case,
the result includes the other endpoint as well so it can be used
again. Below is an example of using `select2`.

~~~
match select2(a, b) {
  left((none, b)) {
    // endpoint a was closed.
  }
  right((a, none)) {
    // endpoint b was closed.
  }
  left((some(_), b)) {
    // endpoint a received a message
  }
  right(a, some(_)) {
    // endpoint b received a message.
  }
}
~~~

Sometimes messages will be available on both endpoints at once. In
this case, `select2` may return either `left` or `right`.

*/
fn select2<A: send, Ab: send, B: send, Bb: send>(
    +a: recv_packet_buffered<A, Ab>,
    +b: recv_packet_buffered<B, Bb>)
    -> either<(option<A>, recv_packet_buffered<B, Bb>),
              (recv_packet_buffered<A, Ab>, option<B>)>
{
    let i = wait_many([a.header(), b.header()]/_);

    unsafe {
        alt i {
          0 { left((try_recv(a), b)) }
          1 { right((a, try_recv(b))) }
          _ { fail ~"select2 return an invalid packet" }
        }
    }
}

trait selectable {
    pure fn header() -> *packet_header;
}

fn selecti<T: selectable>(endpoints: &[T]) -> uint {
    wait_many(endpoints.map(|p| p.header()))
}

fn select2i<A: selectable, B: selectable>(a: A, b: B) -> either<(), ()> {
    alt wait_many([a.header(), b.header()]/_) {
      0 { left(()) }
      1 { right(()) }
      _ { fail ~"wait returned unexpected index" }
    }
}

#[doc = "Waits on a set of endpoints. Returns a message, its index,
 and a list of the remaining endpoints."]
fn select<T: send, Tb: send>(+endpoints: ~[recv_packet_buffered<T, Tb>])
    -> (uint, option<T>, ~[recv_packet_buffered<T, Tb>])
{
    let ready = wait_many(endpoints.map(|p| p.header()));
    let mut remaining = ~[];
    let mut result = none;
    do vec::consume(endpoints) |i, p| {
        if i == ready {
            result = try_recv(p);
        }
        else {
            vec::push(remaining, p);
        }
    }

    (ready, result, remaining)
}

/// The sending end of a pipe. It can be used to send exactly one
/// message.
type send_packet<T: send> = send_packet_buffered<T, packet<T>>;

#[doc(hidden)]
fn send_packet<T: send>(p: *packet<T>) -> send_packet<T> {
    send_packet_buffered(p)
}

struct send_packet_buffered<T: send, Tbuffer: send> {
    let mut p: option<*packet<T>>;
    let mut buffer: option<buffer_resource<Tbuffer>>;
    new(p: *packet<T>) {
        //debug!{"take send %?", p};
        self.p = some(p);
        unsafe {
            self.buffer = some(
                buffer_resource(
                    get_buffer(ptr::addr_of((*p).header))));
        };
    }
    drop {
        //if self.p != none {
        //    debug!{"drop send %?", option::get(self.p)};
        //}
        if self.p != none {
            let mut p = none;
            p <-> self.p;
            sender_terminate(option::unwrap(p))
        }
        //unsafe { error!{"send_drop: %?",
        //                if self.buffer == none {
        //                    "none"
        //                } else { "some" }}; }
    }
    fn unwrap() -> *packet<T> {
        let mut p = none;
        p <-> self.p;
        option::unwrap(p)
    }

    pure fn header() -> *packet_header {
        alt self.p {
          some(packet) {
            unsafe {
                let packet = &*packet;
                let header = ptr::addr_of(packet.header);
                //forget(packet);
                header
            }
          }
          none { fail ~"packet already consumed" }
        }
    }

    fn reuse_buffer() -> buffer_resource<Tbuffer> {
        //error!{"send reuse_buffer"};
        let mut tmp = none;
        tmp <-> self.buffer;
        option::unwrap(tmp)
    }
}

/// Represents the receive end of a pipe. It can receive exactly one
/// message.
type recv_packet<T: send> = recv_packet_buffered<T, packet<T>>;

#[doc(hidden)]
fn recv_packet<T: send>(p: *packet<T>) -> recv_packet<T> {
    recv_packet_buffered(p)
}

struct recv_packet_buffered<T: send, Tbuffer: send> : selectable {
    let mut p: option<*packet<T>>;
    let mut buffer: option<buffer_resource<Tbuffer>>;
    new(p: *packet<T>) {
        //debug!{"take recv %?", p};
        self.p = some(p);
        unsafe {
            self.buffer = some(
                buffer_resource(
                    get_buffer(ptr::addr_of((*p).header))));
        };
    }
    drop {
        //if self.p != none {
        //    debug!{"drop recv %?", option::get(self.p)};
        //}
        if self.p != none {
            let mut p = none;
            p <-> self.p;
            receiver_terminate(option::unwrap(p))
        }
        //unsafe { error!{"recv_drop: %?",
        //                if self.buffer == none {
        //                    "none"
        //                } else { "some" }}; }
    }
    fn unwrap() -> *packet<T> {
        let mut p = none;
        p <-> self.p;
        option::unwrap(p)
    }

    pure fn header() -> *packet_header {
        alt self.p {
          some(packet) {
            unsafe {
                let packet = &*packet;
                let header = ptr::addr_of(packet.header);
                //forget(packet);
                header
            }
          }
          none { fail ~"packet already consumed" }
        }
    }

    fn reuse_buffer() -> buffer_resource<Tbuffer> {
        //error!{"recv reuse_buffer"};
        let mut tmp = none;
        tmp <-> self.buffer;
        option::unwrap(tmp)
    }
}

#[doc(hidden)]
fn entangle<T: send>() -> (send_packet<T>, recv_packet<T>) {
    let p = packet();
    (send_packet(p), recv_packet(p))
}

fn spawn_service<T: send, Tb: send>(
    init: extern fn() -> (send_packet_buffered<T, Tb>,
                          recv_packet_buffered<T, Tb>),
    +service: fn~(+recv_packet_buffered<T, Tb>))
    -> send_packet_buffered<T, Tb>
{
    let (client, server) = init();

    // This is some nasty gymnastics required to safely move the pipe
    // into a new task.
    let server = ~mut some(server);
    do task::spawn |move service| {
        let mut server_ = none;
        server_ <-> *server;
        service(option::unwrap(server_))
    }

    client
}

fn spawn_service_recv<T: send, Tb: send>(
    init: extern fn() -> (recv_packet_buffered<T, Tb>,
                          send_packet_buffered<T, Tb>),
    +service: fn~(+send_packet_buffered<T, Tb>))
    -> recv_packet_buffered<T, Tb>
{
    let (client, server) = init();

    // This is some nasty gymnastics required to safely move the pipe
    // into a new task.
    let server = ~mut some(server);
    do task::spawn |move service| {
        let mut server_ = none;
        server_ <-> *server;
        service(option::unwrap(server_))
    }

    client
}

// Streams - Make pipes a little easier in general.

proto! streamp {
    open:send<T: send> {
        data(T) -> open<T>
    }
}

// It'd be nice to call this send, but it'd conflict with the built in
// send kind.
trait channel<T: send> {
    fn send(+x: T);
}

trait recv<T: send> {
    fn recv() -> T;
    fn try_recv() -> option<T>;
    // This should perhaps be a new trait
    pure fn peek() -> bool;
}

#[doc(hidden)]
type chan_<T:send> = { mut endp: option<streamp::client::open<T>> };

enum chan<T:send> {
    chan_(chan_<T>)
}

#[doc(hidden)]
type port_<T:send> = { mut endp: option<streamp::server::open<T>> };

enum port<T:send> {
    port_(port_<T>)
}

fn stream<T:send>() -> (chan<T>, port<T>) {
    let (c, s) = streamp::init();

    (chan_({ mut endp: some(c) }), port_({ mut endp: some(s) }))
}

impl chan<T: send> of channel<T> for chan<T> {
    fn send(+x: T) {
        let mut endp = none;
        endp <-> self.endp;
        self.endp = some(
            streamp::client::data(unwrap(endp), x))
    }
}

impl port<T: send> of recv<T> for port<T> {
    fn recv() -> T {
        let mut endp = none;
        endp <-> self.endp;
        let streamp::data(x, endp) = pipes::recv(unwrap(endp));
        self.endp = some(endp);
        x
    }

    fn try_recv() -> option<T> {
        let mut endp = none;
        endp <-> self.endp;
        alt move pipes::try_recv(unwrap(endp)) {
          some(streamp::data(x, endp)) {
            self.endp = some(move_it!{endp});
            some(move_it!{x})
          }
          none { none }
        }
    }

    pure fn peek() -> bool unchecked {
        let mut endp = none;
        endp <-> self.endp;
        let peek = alt endp {
          some(endp) {
            pipes::peek(endp)
          }
          none { fail ~"peeking empty stream" }
        };
        self.endp <-> endp;
        peek
    }
}

// Treat a whole bunch of ports as one.
struct port_set<T: send> : recv<T> {
    let mut ports: ~[pipes::port<T>];

    new() { self.ports = ~[]; }

    fn add(+port: pipes::port<T>) {
        vec::push(self.ports, port)
    }

    fn chan() -> chan<T> {
        let (ch, po) = stream();
        self.add(po);
        ch
    }

    fn try_recv() -> option<T> {
        let mut result = none;
        // we have to swap the ports array so we aren't borrowing
        // aliasable mutable memory.
        let mut ports = ~[];
        ports <-> self.ports;
        while result == none && ports.len() > 0 {
            let i = wait_many(ports.map(|p| p.header()));
            alt move ports[i].try_recv() {
                some(m) {
                    result = some(move m);
                }
                none {
                    // Remove this port.
                    let mut ports_ = ~[];
                    ports <-> ports_;
                    vec::consume(ports_,
                                 |j, x| if i != j {
                                     vec::push(ports, x)
                                 });
                }
            }
        }
        ports <-> self.ports;
        result
    }

    fn recv() -> T {
        match move self.try_recv() {
            some(x) { move x }
            none { fail ~"port_set: endpoints closed" }
        }
    }

    pure fn peek() -> bool {
        // It'd be nice to use self.port.each, but that version isn't
        // pure.
        for vec::each(self.ports) |p| {
            if p.peek() { return true }
        }
        false
    }
}

impl<T: send> of selectable for port<T> {
    pure fn header() -> *packet_header unchecked {
        alt self.endp {
          some(endp) {
            endp.header()
          }
          none { fail ~"peeking empty stream" }
        }
    }
}


type shared_chan<T: send> = arc::exclusive<chan<T>>;

impl chan<T: send> of channel<T> for shared_chan<T> {
    fn send(+x: T) {
        let mut xx = some(x);
        do self.with |_c, chan| {
            let mut x = none;
            x <-> xx;
            chan.send(option::unwrap(x))
        }
    }
}

fn shared_chan<T:send>(+c: chan<T>) -> shared_chan<T> {
    arc::exclusive(c)
}

trait select2<T: send, U: send> {
    fn try_select() -> either<option<T>, option<U>>;
    fn select() -> either<T, U>;
}

impl<T: send, U: send, Left: selectable recv<T>, Right: selectable recv<U>>
    of select2<T, U> for (Left, Right) {

    fn select() -> either<T, U> {
        alt self {
          (lp, rp) {
            alt select2i(lp, rp) {
              left(())  { left (lp.recv()) }
              right(()) { right(rp.recv()) }
            }
          }
        }
    }

    fn try_select() -> either<option<T>, option<U>> {
        alt self {
          (lp, rp) {
            alt select2i(lp, rp) {
              left(())  { left (lp.try_recv()) }
              right(()) { right(rp.try_recv()) }
            }
          }
        }
    }
}

#[cfg(test)]
mod test {
    #[test]
    fn test_select2() {
        let (c1, p1) = pipes::stream();
        let (c2, p2) = pipes::stream();

        c1.send("abc");

        alt (p1, p2).select() {
          right(_) { fail }
          _ { }
        }

        c2.send(123);
    }
}
