use crate::embedding::VectorSpace;
use crate::{
    embedding::get_embeddings,
    embedding::Embedding,
    sample::structured_parser::{ParseStatus, ParseStream, Validate},
};
use floneumin_streams::sender::ChannelTextStream;
use llm::Tokenizer;
use llm_samplers::prelude::*;

use llm::{InferenceFeedback, InferenceParameters, InferenceRequest, InferenceResponse, Model};
use std::fmt::Debug;
use std::sync::Mutex;
use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{Arc, RwLock},
};

pub struct LocalSession<S: VectorSpace> {
    task_sender: tokio::sync::mpsc::UnboundedSender<Task<S>>,
    thread_handle: Option<std::thread::JoinHandle<()>>,
    tokenizer: Arc<Tokenizer>,
}

impl<S: VectorSpace> Drop for LocalSession<S> {
    fn drop(&mut self) {
        self.task_sender.send(Task::Kill).unwrap();
        self.thread_handle.take().unwrap().join().unwrap();
    }
}

impl<S: VectorSpace> Debug for LocalSession<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalSession").finish()
    }
}

impl<S: VectorSpace + Send + Sync + 'static> LocalSession<S> {
    pub fn new(model: Box<dyn Model>, session: llm::InferenceSession) -> Self {
        let (task_sender, mut task_receiver) = tokio::sync::mpsc::unbounded_channel();
        let arc_tokenizer = Arc::new(match model.tokenizer() {
            llm::Tokenizer::Embedded(embedded) => llm::Tokenizer::Embedded(embedded.clone()),
            llm::Tokenizer::HuggingFace(hugging_face) => {
                llm::Tokenizer::HuggingFace(hugging_face.clone())
            }
        });

        let thread_handle = std::thread::spawn(move || {
            let mut inner = LocalSessionInner {
                model,
                session,
                embedding_cache: RwLock::new(HashMap::new()),
            };
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    while let Some(task) = task_receiver.recv().await {
                        match task {
                            Task::Kill => break,
                            Task::Infer {
                                prompt,
                                generation_parameters,
                                sender,
                            } => {
                                inner._infer(prompt, generation_parameters, sender);
                            }
                            Task::InferSampler {
                                prompt,
                                max_tokens,
                                sampler,
                                sender,
                            } => {
                                inner._infer_sampler(prompt, max_tokens, sampler, sender);
                            }
                            Task::GetEmbedding { text, sender } => {
                                let result = inner._get_embedding(&text).unwrap();
                                sender.send(Ok(result)).unwrap();
                            }
                        }
                    }
                })
        });
        Self {
            task_sender,
            thread_handle: Some(thread_handle),
            tokenizer: arc_tokenizer,
        }
    }

    pub fn get_tokenizer(&self) -> Arc<Tokenizer> {
        self.tokenizer.clone()
    }

    pub(crate) async fn infer(
        &mut self,
        prompt: String,
        generation_parameters: crate::model::GenerationParameters,
    ) -> ChannelTextStream<String> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.task_sender
            .send(Task::Infer {
                prompt,
                generation_parameters,
                sender,
            })
            .unwrap();
        receiver.await.unwrap()
    }

    pub(crate) async fn infer_sampler(
        &mut self,
        prompt: String,
        max_tokens: Option<u32>,
        sampler: Arc<Mutex<dyn Sampler<u32, f32>>>,
    ) -> ChannelTextStream<String> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.task_sender
            .send(Task::InferSampler {
                prompt,
                max_tokens,
                sampler,
                sender,
            })
            .unwrap();
        receiver.await.unwrap()
    }

    pub(crate) async fn get_embedding(&self, text: &str) -> anyhow::Result<Embedding<S>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.task_sender
            .send(Task::GetEmbedding {
                text: text.to_string(),
                sender,
            })
            .unwrap();
        receiver
            .await
            .unwrap()
            .map_err(|_| anyhow::anyhow!("Failed to receive result"))
    }
}

#[derive(Clone)]
pub struct ArcValidate(pub(crate) Arc<dyn for<'a> Validate<'a> + Send + Sync + 'static>);

impl<'a> Validate<'a> for ArcValidate {
    fn validate(&self, tokens: ParseStream<'a>) -> ParseStatus<'a> {
        self.0.validate(tokens)
    }
}

enum Task<S: VectorSpace> {
    Kill,
    Infer {
        prompt: String,
        generation_parameters: crate::model::GenerationParameters,
        sender: tokio::sync::oneshot::Sender<ChannelTextStream<String>>,
    },
    InferSampler {
        prompt: String,
        max_tokens: Option<u32>,
        sampler: Arc<Mutex<dyn Sampler<u32, f32>>>,
        sender: tokio::sync::oneshot::Sender<ChannelTextStream<String>>,
    },
    GetEmbedding {
        text: String,
        sender: tokio::sync::oneshot::Sender<anyhow::Result<Embedding<S>>>,
    },
}

struct LocalSessionInner<S: VectorSpace> {
    model: Box<dyn Model>,
    session: llm::InferenceSession,
    embedding_cache: RwLock<HashMap<String, Embedding<S>>>,
}

impl<S: VectorSpace> Debug for LocalSessionInner<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalSessionInner").finish()
    }
}

impl<S: VectorSpace> LocalSessionInner<S> {
    fn _infer_sampler(
        &mut self,
        prompt: String,
        max_tokens: Option<u32>,
        sampler: Arc<Mutex<dyn Sampler<u32, f32>>>,
        out: tokio::sync::oneshot::Sender<ChannelTextStream<String>>,
    ) {
        let session = &mut self.session;
        let model = &mut *self.model;

        let parameters = InferenceParameters { sampler };

        let (callback, stream) = inference_callback();
        if let Err(_) = out.send(stream) {
            log::error!("Failed to send stream");
            return;
        }

        let mut rng = rand::thread_rng();

        let request = InferenceRequest {
            prompt: (&prompt).into(),
            parameters: &parameters,
            play_back_previous_tokens: false,
            maximum_token_count: max_tokens.map(|x| x as usize),
        };

        if let Err(err) =
            session.infer(model, &mut rng, &request, &mut Default::default(), callback)
        {
            log::error!("{err}")
        }
    }

    #[tracing::instrument(skip(out))]
    fn _infer(
        &mut self,
        prompt: String,
        generation_parameters: crate::model::GenerationParameters,
        out: tokio::sync::oneshot::Sender<ChannelTextStream<String>>,
    ) {
        let session = &mut self.session;
        let model = &mut *self.model;

        let maximum_token_count = Some(generation_parameters.max_length as usize);

        let parameters = InferenceParameters {
            sampler: Arc::new(Mutex::new(generation_parameters.sampler())),
        };

        let (callback, stream) = inference_callback();
        if let Err(_) = out.send(stream) {
            log::error!("Failed to send stream");
            return;
        }

        let mut rng = rand::thread_rng();

        let request = InferenceRequest {
            prompt: (&prompt).into(),
            parameters: &parameters,
            play_back_previous_tokens: false,
            maximum_token_count,
        };

        if let Err(err) =
            session.infer(model, &mut rng, &request, &mut Default::default(), callback)
        {
            log::error!("{err}")
        }
    }

    #[tracing::instrument]
    fn _get_embedding(&self, text: &str) -> anyhow::Result<Embedding<S>> {
        let mut write = self.embedding_cache.write().unwrap();
        let cache = &mut *write;
        Ok(if let Some(embedding) = cache.get(text) {
            embedding.clone()
        } else {
            let model = self.model.as_ref();
            let new_embedding = get_embeddings(model, text);
            cache.insert(text.to_string(), new_embedding.clone());
            new_embedding
        })
    }
}

fn inference_callback() -> (
    impl FnMut(InferenceResponse) -> Result<InferenceFeedback, Infallible>,
    ChannelTextStream<String>,
) {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let stream = receiver.into();
    let callback = move |resp| match resp {
        InferenceResponse::InferredToken(t) => match sender.send(t) {
            Ok(_) => Ok(InferenceFeedback::Continue),
            Err(_) => {
                log::error!("Failed to send token");
                Ok(InferenceFeedback::Halt)
            }
        },
        InferenceResponse::EotToken => Ok(InferenceFeedback::Halt),
        _ => Ok(InferenceFeedback::Continue),
    };
    (callback, stream)
}
