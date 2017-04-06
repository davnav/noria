#[macro_use]
extern crate clap;

extern crate slog;
extern crate slog_term;

extern crate rand;

extern crate distributary;

extern crate hdrsample;

extern crate spmc;

mod exercise;
mod common;
mod graph;

use common::{Writer, Reader, ArticleResult, Period, MigrationHandle};
use distributary::{Mutator, DataType};

use std::sync::mpsc;
use std::thread;
use std::sync;
use std::time;

fn main() {
    use clap::{Arg, App};

    let args = App::new("vote")
        .version("0.1")
        .about("Benchmarks user-curated news aggregator throughput for in-memory Soup")
        .arg(Arg::with_name("avg")
            .long("avg")
            .takes_value(false)
            .help("compute average throughput at the end of benchmark"))
        .arg(Arg::with_name("cdf")
            .short("c")
            .long("cdf")
            .takes_value(false)
            .help("produce a CDF of recorded latencies for each client at the end"))
        .arg(Arg::with_name("stage")
            .short("s")
            .long("stage")
            .takes_value(false)
            .help("stage execution such that all writes are performed before all reads"))
        .arg(Arg::with_name("ngetters")
            .short("g")
            .long("getters")
            .value_name("N")
            .default_value("1")
            .help("Number of GET clients to start"))
        .arg(Arg::with_name("narticles")
            .short("a")
            .long("articles")
            .value_name("N")
            .default_value("100000")
            .help("Number of articles to prepopulate the database with"))
        .arg(Arg::with_name("runtime")
            .short("r")
            .long("runtime")
            .value_name("N")
            .default_value("60")
            .help("Benchmark runtime in seconds"))
        .arg(Arg::with_name("migrate")
            .short("m")
            .long("migrate")
            .value_name("N")
            .help("Perform a migration after this many seconds")
            .conflicts_with("stage"))
        .get_matches();

    let avg = args.is_present("avg");
    let cdf = args.is_present("cdf");
    let stage = args.is_present("stage");
    let runtime = time::Duration::from_secs(value_t_or_exit!(args, "runtime", u64));
    let migrate_after = args.value_of("migrate")
        .map(|_| value_t_or_exit!(args, "migrate", u64))
        .map(time::Duration::from_secs);
    let ngetters = value_t_or_exit!(args, "ngetters", usize);
    let narticles = value_t_or_exit!(args, "narticles", isize);
    assert!(ngetters > 0);

    if let Some(ref migrate_after) = migrate_after {
        assert!(migrate_after < &runtime);
    }

    let mut config = exercise::RuntimeConfig::new(narticles, runtime);
    config.produce_cdf(cdf);
    if let Some(migrate_after) = migrate_after {
        config.perform_migration_at(migrate_after);
    }

    // setup db
    let g = graph::make();
    let putters = (g.graph.get_mutator(g.article), g.graph.get_mutator(g.vote));

    // prepare getters
    let getters: Vec<_> = (0..ngetters)
        .into_iter()
        .map(|_| Getter::new(g.graph.get_getter(g.end).unwrap()))
        .collect();

    let g = sync::Arc::new(sync::Mutex::new(g));
    let putter = Spoon {
        graph: g.clone(),
        getters: getters.iter().map(|g| unsafe { g.clone() }).collect(),
        article: putters.0,
        vote: putters.1,
        new_vote: None,
        i: 0,
    };

    let put_stats;
    let get_stats: Vec<_>;
    if stage {
        // put then get
        put_stats = exercise::launch_writer(putter, config, None);
        let getters: Vec<_> = getters.into_iter()
            .enumerate()
            .map(|(i, g)| {
                     thread::Builder::new()
                         .name(format!("GET{}", i))
                         .spawn(move || exercise::launch_reader(g, config))
                         .unwrap()
                 })
            .collect();
        get_stats = getters.into_iter().map(|jh| jh.join().unwrap()).collect();
    } else {
        // put & get
        // TODO: how do we start getters after prepopulate?
        let (tx, prepop) = mpsc::sync_channel(0);
        let putter = thread::Builder::new()
            .name("PUT0".to_string())
            .spawn(move || exercise::launch_writer(putter, config, Some(tx)))
            .unwrap();

        // wait for prepopulation to finish
        prepop.recv().is_err();

        let getters: Vec<_> = getters.into_iter()
            .enumerate()
            .map(|(i, g)| {
                     thread::Builder::new()
                         .name(format!("GET{}", i))
                         .spawn(move || exercise::launch_reader(g, config))
                         .unwrap()
                 })
            .collect();

        put_stats = putter.join().unwrap();
        get_stats = getters.into_iter().map(|jh| jh.join().unwrap()).collect();
    }

    print_stats("PUT", &put_stats.pre, avg);
    for (i, s) in get_stats.iter().enumerate() {
        print_stats(format!("GET{}", i), &s.pre, avg);
    }
    if avg {
        let sum = get_stats.iter().fold((0f64, 0usize), |(tot, count), stats| {
            // TODO: do we *really* want an average of averages?
            let (sum, num) = stats.pre.sum_len();
            (tot + sum, count + num)
        });
        println!("avg GET: {:.2}", sum.0 as f64 / sum.1 as f64);
    }

    if migrate_after.is_some() {
        print_stats("PUT+", &put_stats.post, avg);
        for (i, s) in get_stats.iter().enumerate() {
            print_stats(format!("GET{}+", i), &s.post, avg);
        }
        if avg {
            let sum = get_stats.iter().fold((0f64, 0usize), |(tot, count), stats| {
                // TODO: do we *really* want an average of averages?
                let (sum, num) = stats.post.sum_len();
                (tot + sum, count + num)
            });
            println!("avg GET+: {:.2}", sum.0 as f64 / sum.1 as f64);
        }
    }
}

fn print_stats<S: AsRef<str>>(desc: S, stats: &exercise::BenchmarkResult, avg: bool) {
    if let Some(perc) = stats.cdf_percentiles() {
        for (v, p, _, _) in perc {
            println!("percentile {} {:.2} {:.2}", desc.as_ref(), v, p);
        }
    }
    if avg {
        println!("avg {}: {:.2}", desc.as_ref(), stats.avg_throughput());
    }
}

type G = Box<Fn(&DataType) -> Result<Vec<Vec<DataType>>, ()> + Send + 'static>;

// A more dangerous AtomicPtr that also derefs into the inner type
use std::sync::atomic::AtomicPtr;
struct Getter {
    real: sync::Arc<AtomicPtr<G>>,
    used: sync::Arc<AtomicPtr<G>>,
    swapped: bool,
    callable: bool,
}

impl Getter {
    pub fn new(inner: G) -> Getter {
        let m = Box::into_raw(Box::new(inner));
        Getter {
            real: sync::Arc::new(AtomicPtr::new(m)),
            used: sync::Arc::new(AtomicPtr::new(m)),
            swapped: false,
            callable: true,
        }
    }

    // unsafe, because cannot use call() on clones
    pub unsafe fn clone(&self) -> Self {
        Getter {
            real: self.real.clone(),
            used: self.used.clone(),
            swapped: false,
            callable: false,
        }
    }

    // unsafe, because may only be called once
    pub unsafe fn replace(&mut self, new: G) {
        let m = Box::into_raw(Box::new(new));
        self.real.store(m, sync::atomic::Ordering::Release);
    }

    pub fn same(&self) -> bool {
        !self.swapped
    }

    pub fn call(&mut self) -> &mut G {
        assert!(self.callable);

        use std::mem;
        let mut used = self.used.load(sync::atomic::Ordering::Relaxed);
        if !self.swapped {
            let real = self.real.load(sync::atomic::Ordering::Acquire);
            if used != real {
                // replace() happened
                self.used.store(real, sync::atomic::Ordering::Relaxed);
                // need to free the old used
                drop(unsafe { Box::from_raw(used) });
                // no need to check again
                self.swapped = true;
                // use the new pointer
                used = real;
            }
        }
        unsafe { mem::transmute(used) }
    }
}

impl Drop for Getter {
    fn drop(&mut self) {
        if self.callable {
            let used = self.used.load(sync::atomic::Ordering::Acquire);
            let real = self.real.load(sync::atomic::Ordering::Acquire);
            assert_eq!(used, real);
            drop(unsafe { Box::from_raw(used) });
        }
    }
}

struct Spoon {
    graph: sync::Arc<sync::Mutex<graph::Graph>>,
    article: Mutator,
    getters: Vec<Getter>,
    vote: Mutator,
    i: usize,
    new_vote: Option<mpsc::Receiver<Mutator>>,
}

impl Writer for Spoon {
    type Migrator = Migrator;

    fn make_article(&mut self, article_id: i64, title: String) {
        self.article.put(vec![article_id.into(), title.into()]);
    }

    fn vote(&mut self, user_id: i64, article_id: i64) -> Period {
        if self.new_vote.is_some() {
            self.i += 1;
            // don't try too eagerly
            if self.i & 16384 == 0 {
                // we may have been given a new putter
                if let Ok(nv) = self.new_vote
                       .as_mut()
                       .unwrap()
                       .try_recv() {
                    // yay!
                    self.new_vote = None;
                    self.vote = nv;
                    self.i = usize::max_value();
                }
            }
        }

        if self.i == usize::max_value() {
            self.vote.put(vec![user_id.into(), article_id.into(), 5.into()]);
            Period::PostMigration
        } else {
            self.vote.put(vec![user_id.into(), article_id.into()]);
            Period::PreMigration
        }
    }

    fn prepare_migration(&mut self) -> Self::Migrator {
        let (tx, rx) = mpsc::sync_channel(0);
        self.new_vote = Some(rx);
        self.i = 1;
        Migrator {
            graph: self.graph.clone(),
            mut_tx: tx,
            getters: self.getters
                .iter()
                .map(|g| unsafe { g.clone() })
                .collect(),
        }
    }
}

struct Migrator {
    graph: sync::Arc<sync::Mutex<graph::Graph>>,
    mut_tx: mpsc::SyncSender<Mutator>,
    getters: Vec<Getter>,
}

impl MigrationHandle for Migrator {
    fn execute(&mut self) {
        use std::collections::HashMap;
        use distributary::{Base, Aggregation, Join, JoinType, Union};

        let mut g = self.graph.lock().unwrap();
        let (rating, newend) = {
            // get all the ids since migration will borrow g
            let vc = g.vc;
            let article = g.article;

            // migrate
            let mut mig = g.graph.start_migration();

            // add new "ratings" base table
            let rating = mig.add_ingredient("rating", &["user", "id", "stars"], Base::default());

            // add sum of ratings
            let rs = mig.add_ingredient("rsum",
                                        &["id", "total"],
                                        Aggregation::SUM.over(rating, 2, &[1]));

            // take a union of vote count and rsum
            let mut emits = HashMap::new();
            emits.insert(rs, vec![0, 1]);
            emits.insert(vc, vec![0, 1]);
            let u = Union::new(emits);
            let both = mig.add_ingredient("both", &["id", "value"], u);

            // sum them by article id
            let total = mig.add_ingredient("total",
                                           &["id", "total"],
                                           Aggregation::SUM.over(both, 1, &[0]));

            // finally, produce end result
            use distributary::JoinSource::*;
            let j = Join::new(article, total, JoinType::Inner, vec![B(0, 0), L(1), R(1)]);
            let newend = mig.add_ingredient("awr", &["id", "title", "score"], j);
            mig.maintain(newend, 0);

            // we want ratings, rsum, and the union to be in the same domain,
            // because only rsum is really costly
            let domain = mig.add_domain();
            mig.assign_domain(rating, domain);
            mig.assign_domain(rs, domain);
            mig.assign_domain(both, domain);

            // and then we want the total sum and the join in the same domain,
            // to avoid duplicating the total state
            let domain = mig.add_domain();
            mig.assign_domain(total, domain);
            mig.assign_domain(newend, domain);

            // start processing
            mig.commit();
            (rating, newend)
        };

        let mutator = g.graph.get_mutator(rating);
        self.mut_tx.send(mutator).unwrap();
        for getter in &mut self.getters {
            unsafe { getter.replace(g.graph.get_getter(newend).unwrap()) };
        }
    }
}

impl Reader for Getter {
    fn get(&mut self, article_id: i64) -> (ArticleResult, Period) {
        let id = article_id.into();
        let res = match (self.call())(&id) {
            Ok(g) => {
                match g.into_iter().next() {
                    Some(row) => {
                        // we only care about the first result
                        let mut row = row.into_iter();
                        let id: i64 = row.next().unwrap().into();
                        let title: String = row.next().unwrap().into();
                        let count: i64 = row.next().unwrap().into();
                        ArticleResult::Article {
                            id: id,
                            title: title,
                            votes: count,
                        }
                    }
                    None => ArticleResult::NoSuchArticle,
                }
            }
            Err(_) => ArticleResult::Error,
        };

        if self.same() {
            (res, Period::PreMigration)
        } else {
            (res, Period::PostMigration)
        }
    }
}
