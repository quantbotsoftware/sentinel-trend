use std::sync::Arc;
use tokio::sync::mpsc::{self, Receiver, Sender};
use crate::smarttrend::memory_db::MemoryDb;
use crate::smarttrend::strategies::MarketRegime;

pub enum MlLogMessage {
    RecordSample {
        feature_vec: Vec<f64>,
        regime: MarketRegime,
        ts_open: i64,
        symbol: String,
        setup_id: i64,
        source: String,
        response_tx: tokio::sync::oneshot::Sender<i64>,
    },
    UpdateReturn {
        id: i64,
        return_norm: f64,
        ts_close: i64,
    },
}

#[derive(Clone)]
pub struct MlLogger {
    tx: Sender<MlLogMessage>,
}

impl MlLogger {
    pub fn new(memory_db: Arc<MemoryDb>) -> Self {
        let (tx, rx) = mpsc::channel(1000);
        
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    Self::run_worker(rx, memory_db).await;
                });
            }
            Err(_) => {
                std::thread::spawn(move || {
                    if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        rt.block_on(async move {
                            Self::run_worker(rx, memory_db).await;
                        });
                    }
                });
            }
        }

        Self { tx }
    }

    async fn run_worker(mut rx: Receiver<MlLogMessage>, db: Arc<MemoryDb>) {
        tracing::info!(component = "ml_logger", "Asynchronous ML Logger worker started");
        while let Some(msg) = rx.recv().await {
            match msg {
                MlLogMessage::RecordSample {
                    feature_vec,
                    regime,
                    ts_open,
                    symbol,
                    setup_id,
                    source,
                    response_tx,
                } => {
                    let db_clone = Arc::clone(&db);
                    let res = tokio::task::spawn_blocking(move || {
                        db_clone.record_ml_sample(
                            &feature_vec,
                            0.0,
                            regime,
                            ts_open,
                            0,
                            &symbol,
                            Some(setup_id),
                            &source,
                        )
                    })
                    .await;

                    let row_id = match res {
                        Ok(Ok(id)) => id,
                        Ok(Err(e)) => {
                            tracing::error!(component = "ml_logger", "Failed to insert ML sample: {e}");
                            -1
                        }
                        Err(e) => {
                            tracing::error!(component = "ml_logger", "ML sample insert task panicked: {e}");
                            -1
                        }
                    };

                    let _ = response_tx.send(row_id);
                }
                MlLogMessage::UpdateReturn { id, return_norm, ts_close } => {
                    let db_clone = Arc::clone(&db);
                    let _ = tokio::task::spawn_blocking(move || {
                        if let Err(e) = db_clone.update_ml_sample_return(id, return_norm, ts_close) {
                            tracing::error!(component = "ml_logger", "Failed to update ML sample return for id {id}: {e}");
                        }
                    })
                    .await;
                }
            }
        }
        tracing::info!(component = "ml_logger", "Asynchronous ML Logger worker exiting");
    }

    pub async fn record_sample(
        &self,
        feature_vec: Vec<f64>,
        regime: MarketRegime,
        ts_open: i64,
        symbol: String,
        setup_id: i64,
        source: String,
    ) -> Result<i64, String> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let msg = MlLogMessage::RecordSample {
            feature_vec,
            regime,
            ts_open,
            symbol,
            setup_id,
            source,
            response_tx,
        };

        if self.tx.send(msg).await.is_err() {
            return Err("ML Logger channel closed".to_string());
        }

        response_rx.await.map_err(|e| format!("Failed to receive ML Logger response: {e}"))
    }

    pub fn update_return(&self, id: i64, return_norm: f64, ts_close: i64) {
        let msg = MlLogMessage::UpdateReturn { id, return_norm, ts_close };
        if let Err(e) = self.tx.try_send(msg) {
            tracing::error!(component = "ml_logger", "Failed to send update return request: {e}");
        }
    }
}
