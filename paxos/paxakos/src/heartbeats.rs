//! The heartbeats decoration continually appends heartbeat entries to the log.
//!
//! Sending heartbeats serves two purposes.
//!
//! 1. It establishes a minimum pace at which state is moving forward. This
//!    enables nodes to detect when they have been disconnected from the
//!    cluster.
//! 2. Heartbeats can automatically refresh a [master
//!    lease][crate::leases::master]. Having a stable master is helpful for any
//!    services that wish to delegate to it.
//!
//! It should be noted that heartbeats uses `Importance::MaintainLeadership` to
//! append heartbeats. As such it will never contend with other nodes that may
//! be in the process of proposing some other entry for the same round.

use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use futures::future::FutureExt;
use futures::future::LocalBoxFuture;
use futures::stream::StreamExt;

use crate::append::AppendArgs;
use crate::append::Importance;
use crate::append::Peeryness;
use crate::applicable::ApplicableTo;
use crate::buffer::Buffer;
use crate::decoration::Decoration;
use crate::error::Disoriented;
use crate::error::ShutDownOr;
use crate::leadership::track::MaybeLeadershipAwareNode;
use crate::node::builder::NodeBuilder;
use crate::node::AbstainOf;
use crate::node::AppendResultFor;
use crate::node::CommunicatorOf;
use crate::node::CoordNumOf;
use crate::node::EventFor;
use crate::node::ImplAppendResultFor;
use crate::node::InvocationOf;
use crate::node::LogEntryOf;
use crate::node::NayOf;
use crate::node::NodeIdOf;
use crate::node::NodeImpl;
use crate::node::NodeStatus;
use crate::node::Participation;
use crate::node::RoundNumOf;
use crate::node::SnapshotFor;
use crate::node::StateOf;
use crate::node::StaticAppendResultFor;
use crate::node::YeaOf;
use crate::retry::DoNotRetry;
use crate::retry::RetryPolicy;
use crate::voting::Voter;
use crate::Node;

/// Heartbeats configuration.
pub trait Config {
    /// The node type that is decorated.
    type Node: Node;

    /// The applicable that is used to fill gaps, usually a no-op.
    type Applicable: ApplicableTo<StateOf<Self::Node>> + 'static;

    /// Initializes this configuration.
    #[allow(unused_variables)]
    fn init(&mut self, node: &Self::Node) {}

    /// Updates the configuration with the given event.
    #[allow(unused_variables)]
    fn update(&mut self, event: &EventFor<Self::Node>) {}

    /// Interval at which leader nodes send heartbeats.
    fn leader_interval(&self) -> Option<Duration> {
        None
    }

    /// Interval at which heartbeats are sent.
    fn interval(&self) -> Option<Duration>;

    /// Creates a new heartbeat value.
    fn new_heartbeat(&self) -> Self::Applicable;
}

/// A static configuration.
pub struct StaticConfig<N, A> {
    leader_interval: Option<Duration>,
    interval: Option<Duration>,

    _p: std::marker::PhantomData<(N, A)>,
}

impl<N, A> StaticConfig<N, A>
where
    N: Node,
    A: ApplicableTo<StateOf<N>> + Default + 'static,
{
    /// Constructs a new configuratin with the given interval.
    pub fn with_interval(interval: Duration) -> Self {
        Self {
            leader_interval: None,
            interval: Some(interval),

            _p: std::marker::PhantomData,
        }
    }

    /// Sets the interval at which leader nodes send heartbeats.
    pub fn when_leading(self, leader_interval: Duration) -> Self {
        Self {
            leader_interval: Some(leader_interval),
            interval: self.interval,

            _p: std::marker::PhantomData,
        }
    }
}

impl<N, A> Config for StaticConfig<N, A>
where
    N: Node,
    A: ApplicableTo<StateOf<N>> + Default + 'static,
{
    type Node = N;
    type Applicable = A;

    fn leader_interval(&self) -> Option<Duration> {
        self.leader_interval
    }

    fn interval(&self) -> Option<Duration> {
        self.interval
    }

    fn new_heartbeat(&self) -> Self::Applicable {
        Self::Applicable::default()
    }
}

/// Extends `NodeBuilder` to conveniently decorate a node with `Heartbeats`.
pub trait HeartbeatsBuilderExt<I = ()> {
    /// Node type to be decorated.
    type Node: MaybeLeadershipAwareNode<I> + 'static;
    /// Voter type.
    type Voter: Voter;
    /// Buffer type.
    type Buffer: Buffer;

    /// Decorates the node with `Heartbeats` using the given configuration.
    #[allow(clippy::type_complexity)]
    fn send_heartbeats<C>(
        self,
        config: C,
    ) -> NodeBuilder<Heartbeats<Self::Node, C, I>, Self::Voter, Self::Buffer>
    where
        C: Config<Node = Self::Node> + 'static;
}

impl<N, V, B, I> HeartbeatsBuilderExt<I> for NodeBuilder<N, V, B>
where
    N: NodeImpl + MaybeLeadershipAwareNode<I> + 'static,
    V: Voter<
        State = StateOf<N>,
        RoundNum = RoundNumOf<N>,
        CoordNum = CoordNumOf<N>,
        Abstain = AbstainOf<N>,
        Yea = YeaOf<N>,
        Nay = NayOf<N>,
    >,
    B: Buffer<RoundNum = RoundNumOf<N>, CoordNum = CoordNumOf<N>, Entry = LogEntryOf<N>>,
{
    type Node = N;
    type Voter = V;
    type Buffer = B;

    #[allow(clippy::type_complexity)]
    fn send_heartbeats<C>(
        self,
        config: C,
    ) -> NodeBuilder<Heartbeats<Self::Node, C, I>, Self::Voter, Self::Buffer>
    where
        C: Config<Node = Self::Node> + 'static,
    {
        self.decorated_with(config)
    }
}

/// Heartbeats decoration.
#[derive(Debug)]
pub struct Heartbeats<N, C, I = ()>
where
    N: MaybeLeadershipAwareNode<I> + 'static,
    C: Config<Node = N>,
{
    decorated: N,
    config: C,

    timer: Option<futures_timer::Delay>,

    appends: futures::stream::FuturesUnordered<LocalBoxFuture<'static, ()>>,

    warned_no_leadership_tracking: bool,

    _p: std::marker::PhantomData<I>,
}

impl<N, C, I> Heartbeats<N, C, I>
where
    N: MaybeLeadershipAwareNode<I> + 'static,
    C: Config<Node = N>,
{
    fn new_timer(&mut self) -> Option<futures_timer::Delay> {
        let delay = match self.decorated.strict_leadership() {
            Some(leadership) => {
                if leadership.first().map(|l| l.leader) == Some(self.id()) {
                    self.config
                        .leader_interval()
                        .or_else(|| self.config.interval())
                } else {
                    self.config.interval()
                }
            }

            None => {
                if self.config.leader_interval().is_some() && self.warned_no_leadership_tracking {
                    self.warned_no_leadership_tracking = true;

                    tracing::warn!(
                        "A leader interval is configured but leadership is not tracked."
                    );
                }

                self.config.interval()
            }
        };

        delay.map(futures_timer::Delay::new)
    }

    fn send_heartbeat(&mut self) {
        let append = self
            .decorated
            .append_static(
                self.config.new_heartbeat(),
                AppendArgs {
                    retry_policy: DoNotRetry::new(),
                    importance: Importance::MaintainLeadership(Peeryness::Peery),
                    ..Default::default()
                },
            )
            .map(|_| ())
            .boxed_local();

        self.appends.push(append);
    }
}

impl<N, C, I> Decoration for Heartbeats<N, C, I>
where
    N: NodeImpl + MaybeLeadershipAwareNode<I> + 'static,
    C: Config<Node = N> + 'static,
{
    type Arguments = C;
    type Decorated = N;

    fn wrap(
        decorated: Self::Decorated,
        mut arguments: Self::Arguments,
    ) -> Result<Self, crate::error::SpawnError> {
        arguments.init(&decorated);

        Ok(Self {
            decorated,
            config: arguments,

            timer: None,

            appends: futures::stream::FuturesUnordered::new(),

            warned_no_leadership_tracking: false,

            _p: std::marker::PhantomData,
        })
    }

    fn peek_into(decorated: &Self) -> &Self::Decorated {
        &decorated.decorated
    }

    fn unwrap(decorated: Self) -> Self::Decorated {
        decorated.decorated
    }
}

impl<N, C, I> Node for Heartbeats<N, C, I>
where
    N: MaybeLeadershipAwareNode<I> + 'static,
    C: Config<Node = N>,
{
    type Invocation = InvocationOf<N>;
    type Communicator = CommunicatorOf<N>;
    type Shutdown = <N as Node>::Shutdown;

    fn id(&self) -> NodeIdOf<Self> {
        self.decorated.id()
    }

    fn status(&self) -> crate::NodeStatus {
        self.decorated.status()
    }

    fn participation(&self) -> Participation<RoundNumOf<Self>> {
        self.decorated.participation()
    }

    fn poll_events(&mut self, cx: &mut std::task::Context<'_>) -> Poll<EventFor<Self>> {
        let event = self.decorated.poll_events(cx);

        if let Poll::Ready(event) = &event {
            self.config.update(event);

            match event {
                crate::Event::Init {
                    status: new_status, ..
                }
                | crate::Event::StatusChange { new_status, .. } => {
                    self.timer = match new_status {
                        NodeStatus::Disoriented => None,
                        _ => self.new_timer(),
                    };
                }

                crate::Event::Install { .. } | crate::Event::Apply { .. } => {
                    self.timer = self.new_timer();
                }

                _ => {}
            }
        }

        while let Some(timer) = &mut self.timer {
            if timer.poll_unpin(cx).is_pending() {
                break;
            }

            self.send_heartbeat();

            self.timer = self.new_timer();
        }

        let _ = self.appends.poll_next_unpin(cx);

        event
    }

    fn handle(&self) -> crate::node::HandleFor<Self> {
        self.decorated.handle()
    }

    fn prepare_snapshot(
        &self,
    ) -> LocalBoxFuture<'static, Result<SnapshotFor<Self>, crate::error::PrepareSnapshotError>>
    {
        self.decorated.prepare_snapshot()
    }

    fn affirm_snapshot(
        &self,
        snapshot: SnapshotFor<Self>,
    ) -> LocalBoxFuture<'static, Result<(), crate::error::AffirmSnapshotError>> {
        self.decorated.affirm_snapshot(snapshot)
    }

    fn install_snapshot(
        &self,
        snapshot: SnapshotFor<Self>,
    ) -> LocalBoxFuture<'static, Result<(), crate::error::InstallSnapshotError>> {
        self.decorated.install_snapshot(snapshot)
    }

    fn read_stale(
        &self,
    ) -> futures::future::LocalBoxFuture<'_, Result<Arc<StateOf<Self>>, Disoriented>> {
        self.decorated.read_stale()
    }

    fn append<A, P, R>(
        &self,
        applicable: A,
        args: P,
    ) -> futures::future::LocalBoxFuture<'_, AppendResultFor<Self, A, R>>
    where
        A: ApplicableTo<StateOf<Self>> + 'static,
        P: Into<AppendArgs<Self::Invocation, R>>,
        R: RetryPolicy<Invocation = Self::Invocation>,
    {
        self.decorated.append(applicable, args)
    }

    fn append_static<A, P, R>(
        &self,
        applicable: A,
        args: P,
    ) -> futures::future::LocalBoxFuture<'static, StaticAppendResultFor<Self, A, R>>
    where
        A: ApplicableTo<StateOf<Self>> + 'static,
        P: Into<AppendArgs<Self::Invocation, R>>,
        R: RetryPolicy<Invocation = Self::Invocation>,
        R::StaticError: From<ShutDownOr<R::Error>>,
    {
        self.decorated.append_static(applicable, args)
    }

    fn shut_down(self) -> Self::Shutdown {
        self.decorated.shut_down()
    }
}

impl<N, C, I> NodeImpl for Heartbeats<N, C, I>
where
    N: NodeImpl + MaybeLeadershipAwareNode<I> + 'static,
    C: Config<Node = N>,
{
    fn append_impl<A, P, R>(
        &self,
        applicable: A,
        args: P,
    ) -> LocalBoxFuture<'static, ImplAppendResultFor<Self, A, R>>
    where
        A: ApplicableTo<StateOf<Self>> + 'static,
        P: Into<AppendArgs<Self::Invocation, R>>,
        R: RetryPolicy<Invocation = Self::Invocation>,
    {
        self.decorated.append_impl(applicable, args)
    }

    fn await_commit_of(
        &self,
        log_entry_id: crate::node::LogEntryIdOf<Self>,
    ) -> LocalBoxFuture<'static, Result<crate::node::CommitFor<Self>, crate::error::ShutDown>> {
        self.decorated.await_commit_of(log_entry_id)
    }
}
