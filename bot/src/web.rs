use webutil::prelude::*;
use webutil::worker::{ Worker, WorkerSender };
use webutil::channel::{ channel, Receiver };
use serde::{ Serialize, de::DeserializeOwned };
use libtetris::*;
use crate::evaluation::Evaluator;
use crate::moves::Move;
use crate::{ Options, Info, AsyncBotState, BotMsg, Thinker, ThinkResult };
use futures_util::{ select, pin_mut };
use futures_util::FutureExt;

// trait aliases (#41517) would make my life SOOOOO much easier
// pub trait WebCompatibleEvaluator = where
//     Self: Evaluator + Clone + Serialize + DeserializeOwned + 'static,
//     <Self as Evaluator>::Reward: Serialize + DeserializeOwned,
//     <Self as Evaluator>::Value: Serialize + DeserializeOwned;

pub struct Interface {
    dead: bool,
    worker: Worker<BotMsg, (Move, Info)>
}

impl Interface {
    /// Launches a bot worker with the specified starting board and options.
    pub async fn launch<E>(
        board: Board,
        options: Options,
        evaluator: E
    ) -> Self
    where
        E: Evaluator + Clone + Serialize + DeserializeOwned + 'static,
        E::Value: Serialize + DeserializeOwned,
        E::Reward: Serialize + DeserializeOwned
    {
        if options.threads == 0 {
            panic!("Invalid number of threads: 0");
        }

        let worker = Worker::new(bot_thread, &(board, options, evaluator)).await.unwrap();

        Interface {
            dead: false,
            worker
        }
    }

    /// Returns true if all possible piece placement sequences result in death.
    pub fn is_dead(&self) -> bool {
        self.dead
    }

    /// Request the bot to provide a move as soon as possible.
    /// 
    /// In most cases, "as soon as possible" is a very short amount of time, and is only longer if
    /// the provided lower limit on thinking has not been reached yet or if the bot cannot provide
    /// a move yet, usually because it lacks information on the next pieces.
    /// 
    /// For example, in a game with zero piece previews and hold enabled, the bot will never be able
    /// to provide the first move because it cannot know what piece it will be placing if it chooses
    /// to hold. Another example: in a game with zero piece previews and hold disabled, the bot
    /// will only be able to provide a move after the current piece spawns and you provide the piece
    /// information to the bot using `add_next_piece`.
    /// 
    /// It is recommended that you call this function the frame before the piece spawns so that the
    /// bot has time to finish its current thinking cycle and supply the move.
    /// 
    /// Once a move is chosen, the bot will update its internal state to the result of the piece
    /// being placed correctly and the move will become available by calling `poll_next_move`.
    pub fn request_next_move(&mut self, incoming: u32) {
        if self.worker.send(&BotMsg::NextMove(incoming)).is_err() {
            self.dead = true;
        }
    }

    /// Checks to see if the bot has provided the previously requested move yet.
    /// 
    /// The returned move contains both a path and the expected location of the placed piece. The
    /// returned path is reasonably good, but you might want to use your own pathfinder to, for
    /// example, exploit movement intricacies in the game you're playing.
    /// 
    /// If the piece couldn't be placed in the expected location, you must call `reset` to reset the
    /// game field, back-to-back status, and combo values.
    pub fn poll_next_move(&mut self) -> Option<(Move, Info)> {
        self.worker.try_recv()
    }

    /// Adds a new piece to the end of the queue.
    /// 
    /// If speculation is enabled, the piece *must* be in the bag. For example, if in the current
    /// bag you've provided the sequence IJOZT, then the next time you call this function you can
    /// only provide either an L or an S piece.
    pub fn add_next_piece(&mut self, piece: Piece) {
        if self.worker.send(&BotMsg::NewPiece(piece)).is_err() {
            self.dead = true;
        }
    }

    /// Resets the playfield, back-to-back status, and combo count.
    /// 
    /// This should only be used when garbage is received or when your client could not place the
    /// piece in the correct position for some reason (e.g. 15 move rule), since this forces the
    /// bot to throw away previous computations.
    /// 
    /// Note: combo is not the same as the displayed combo in guideline games. Here, it is the
    /// number of consecutive line clears achieved. So, generally speaking, if "x Combo" appears
    /// on the screen, you need to use x+1 here.
    pub fn reset(&mut self, field: [[bool; 10]; 40], b2b_active: bool, combo: u32) {
        if self.worker.send(&BotMsg::Reset {
            field, b2b: b2b_active, combo
        }).is_err() {
            self.dead = true;
        }
    }

    /// Specifies a line that Cold Clear should analyze before making any moves.
    pub fn force_analysis_line(&mut self, path: Vec<FallingPiece>) {
        if self.worker.send(&BotMsg::ForceAnalysisLine(path)).is_err() {
            self.dead = true;
        }
    }
}

fn bot_thread<E>(
    (board, options, eval): (Board, Options, E),
    recv: Receiver<BotMsg>,
    send: WorkerSender<(Move, Info)>
) where
    E: Evaluator + Clone + Serialize + DeserializeOwned + 'static,
    E::Value: Serialize + DeserializeOwned,
    E::Reward: Serialize + DeserializeOwned
{
    spawn_local(async move {
        let (result_send, think_recv) = channel::<ThinkResult<E>>();
        let (think_send, thinker_recv) = channel::<Thinker<E>>();
        // spawn thinker workers
        for _ in 0..options.threads {
            let result_send = result_send.clone();
            let thinker_recv = thinker_recv.clone();
            spawn_local(async move {
                let think_worker = Worker::new(thinker, &()).await.unwrap();
                while let Some(thinker) = thinker_recv.recv().await {
                    think_worker.send(&thinker).unwrap();
                    result_send.send(think_worker.recv().await).ok().unwrap();
                }
            });
        }

        let mut state = AsyncBotState::new(board, options, eval);

        while !state.is_dead() {
            let (new_thinks, _) = state.think(|mv, info| send.send(&(mv, info)));
            for thinker in new_thinks {
                think_send.send(thinker).ok().unwrap();
            }

            let msg = recv.recv().fuse();
            let think = think_recv.recv().fuse();
            pin_mut!(msg, think);
            select! {
                msg = msg => match msg {
                    Some(msg) => state.message(msg),
                    None => break
                },
                think = think => state.think_done(think.unwrap())
            }
        }
    });
}

fn thinker<E>(_: (), recv: Receiver<Thinker<E>>, send: WorkerSender<ThinkResult<E>>)
where
    E: Evaluator + Clone + Serialize + DeserializeOwned + 'static,
    E::Value: Serialize + DeserializeOwned,
    E::Reward: Serialize + DeserializeOwned
{
    spawn_local(async move {
        while let Some(v) = recv.recv().await {
            send.send(&v.think());
        }
    })
}