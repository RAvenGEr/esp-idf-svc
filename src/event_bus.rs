use core::fmt::{Debug, Display};
use core::marker::PhantomData;
use core::mem;
use core::ptr;
use core::result::Result;
use core::time::Duration;

extern crate alloc;
use alloc::sync::Arc;

use ::log::*;

use embedded_svc::{event_bus, service};

use esp_idf_hal::cpu::Core;
use esp_idf_hal::delay::TickType;
use esp_idf_hal::mutex;

use esp_idf_sys::*;

use crate::private::cstr::RawCstrs;

pub type EspSystemEventBus = EspEventBus<System>;
pub type EspBackgroundEventBus = EspEventBus<User<Background>>;
pub type EspExplicitEventBus = EspEventBus<User<Explicit>>;
pub type EspPinnedEventBus = EspEventBus<User<Pinned>>;

#[derive(Debug)]
pub struct BackgroundConfiguration<'a> {
    pub queue_size: usize,
    pub task_name: &'a str,
    pub task_priority: u8,
    pub task_stack_size: usize,
    pub task_pin_to_core: Core,
}

impl<'a> Default for BackgroundConfiguration<'a> {
    fn default() -> Self {
        Self {
            queue_size: 8192,
            task_name: "(unknown)",
            task_priority: 0,
            task_stack_size: 3072,
            task_pin_to_core: Core::Core0,
        }
    }
}

impl<'a> From<&BackgroundConfiguration<'a>> for (esp_event_loop_args_t, RawCstrs) {
    fn from(conf: &BackgroundConfiguration<'a>) -> Self {
        let mut rcs = RawCstrs::new();

        let ela = esp_event_loop_args_t {
            queue_size: conf.queue_size as _,
            task_name: rcs.as_ptr(conf.task_name),
            task_priority: conf.task_priority as _,
            task_stack_size: conf.task_stack_size as _,
            task_core_id: conf.task_pin_to_core as _,
        };

        (ela, rcs)
    }
}

#[derive(Debug)]
pub struct Configuration {
    pub queue_size: usize,
}

impl Default for Configuration {
    fn default() -> Self {
        Self { queue_size: 8192 }
    }
}

impl From<&Configuration> for esp_event_loop_args_t {
    fn from(conf: &Configuration) -> Self {
        esp_event_loop_args_t {
            queue_size: conf.queue_size as _,
            ..Default::default()
        }
    }
}

static TAKEN: mutex::Mutex<bool> = mutex::Mutex::new(false);

#[derive(Clone)]
pub struct System;
#[derive(Clone)]
pub struct User<T>(esp_event_loop_handle_t, PhantomData<T>);
#[derive(Clone)]
pub struct Background;
#[derive(Clone)]
pub struct Explicit;
#[derive(Clone)]
pub struct Pinned;

pub trait EspEventBusType {
    fn is_system() -> bool;
}

impl EspEventBusType for System {
    fn is_system() -> bool {
        true
    }
}

impl<T> EspEventBusType for User<T> {
    fn is_system() -> bool {
        false
    }
}

struct UnsafeCallback<P>(*mut Box<dyn FnMut(&P) + 'static>);

impl<P> UnsafeCallback<P> {
    fn call(&self, payload: &P) {
        (unsafe { self.0.as_mut().unwrap() })(payload);
    }
}

unsafe impl<P> Send for UnsafeCallback<P> {}

pub struct EspSubscription<P, T>
where
    T: EspEventBusType,
{
    event_loop_handle: Arc<EventBusHandle<T>>,
    handler_instance: esp_event_handler_instance_t,
    source: event_bus::Source<P>,
    _callback: Box<Box<dyn FnMut(&P) + 'static>>,
}

impl<P, T> EspSubscription<P, T>
where
    T: EspEventBusType,
{
    extern "C" fn handle(
        event_handler_arg: *mut c_types::c_void,
        _event_base: esp_event_base_t,
        _event_id: i32,
        event_data: *mut c_types::c_void,
    ) {
        let callback = unsafe {
            (event_handler_arg as *const UnsafeCallback<P>)
                .as_ref()
                .unwrap()
        };

        let payload: &P = unsafe { (event_data as *const P).as_ref().unwrap() };

        callback.call(payload);
    }
}

unsafe impl<P> Send for EspSubscription<P, System> {}
unsafe impl<P> Send for EspSubscription<P, User<Background>> {}
unsafe impl<P> Send for EspSubscription<P, User<Explicit>> {}

impl<P, T> Drop for EspSubscription<P, T>
where
    T: EspEventBusType,
{
    fn drop(&mut self) {
        let raw_source = as_raw_source_id(&self.source);

        if T::is_system() {
            unsafe {
                esp!(esp_event_handler_instance_unregister(
                    raw_source,
                    ESP_EVENT_ANY_ID,
                    self.handler_instance
                ))
                .unwrap();
            }
        } else {
            unsafe {
                let user: &User<T> = mem::transmute(&*self.event_loop_handle);

                esp!(esp_event_handler_instance_unregister_with(
                    user.0,
                    raw_source,
                    ESP_EVENT_ANY_ID,
                    self.handler_instance
                ))
                .unwrap();
            }
        }
    }
}

impl<P, T> event_bus::Subscription<P> for EspSubscription<P, T> where T: EspEventBusType {}

struct EventBusHandle<T>(T)
where
    T: EspEventBusType;

impl EventBusHandle<System> {
    fn new() -> Result<Self, EspError> {
        let mut taken = TAKEN.lock();

        if *taken {
            esp!(ESP_ERR_INVALID_STATE as i32)?;
        }

        esp!(unsafe { esp_event_loop_create_default() })?;

        *taken = true;

        Ok(Self(System))
    }
}

impl<T> EventBusHandle<User<T>> {
    fn new_internal(conf: &esp_event_loop_args_t) -> Result<Self, EspError> {
        let mut handle: esp_event_loop_handle_t = ptr::null_mut();

        esp!(unsafe { esp_event_loop_create(conf as *const _, &mut handle as _) })?;

        Ok(Self(User(handle, PhantomData)))
    }
}

impl EventBusHandle<User<Background>> {
    fn new(conf: &BackgroundConfiguration) -> Result<Self, EspError> {
        let (nconf, _rcs) = conf.into();

        Self::new_internal(&nconf)
    }
}

impl EventBusHandle<User<Explicit>> {
    fn new(conf: &Configuration) -> Result<Self, EspError> {
        Self::new_internal(&conf.into())
    }
}

impl EventBusHandle<User<Pinned>> {
    fn new(conf: &Configuration) -> Result<Self, EspError> {
        Self::new_internal(&conf.into())
    }
}

impl<T> Drop for EventBusHandle<T>
where
    T: EspEventBusType,
{
    fn drop(&mut self) {
        if T::is_system() {
            let mut taken = TAKEN.lock();

            unsafe {
                esp!(esp_event_loop_delete_default()).unwrap();
            }

            *taken = false;
        } else {
            unsafe {
                let user: &User<T> = mem::transmute(&mut self.0);

                esp!(esp_event_loop_delete(user.0)).unwrap();
            }
        }

        info!("Dropped");
    }
}

#[derive(Clone)]
pub struct EspEventBus<T>(Arc<EventBusHandle<T>>)
where
    T: EspEventBusType;

impl<T> service::Service for EspEventBus<T>
where
    T: EspEventBusType,
{
    type Error = EspError;
}

impl EspEventBus<System> {
    pub fn new() -> Result<Self, EspError> {
        Ok(Self(Arc::new(EventBusHandle::<System>::new()?)))
    }

    fn internal_subscribe<P, E>(
        &self,
        source: event_bus::Source<P>,
        mut callback: impl for<'b> FnMut(&'b P) -> Result<(), E> + 'static,
    ) -> Result<EspSubscription<P, System>, EspError>
    where
        E: Display + Debug + Send + Sync + 'static,
    {
        let raw_source = as_raw_source_id(&source);

        let mut handler_instance: esp_event_handler_instance_t = ptr::null_mut();

        let callback: Box<dyn FnMut(&P) + 'static> =
            Box::new(move |payload| callback(payload).unwrap());
        let mut callback = Box::new(callback);

        let unsafe_callback = UnsafeCallback(&mut *callback as *mut _);

        esp!(unsafe {
            esp_event_handler_instance_register(
                raw_source,
                ESP_EVENT_ANY_ID,
                Some(EspSubscription::<P, System>::handle),
                &unsafe_callback as *const _ as *mut _,
                &mut handler_instance as *mut _,
            )
        })?;

        Ok(EspSubscription {
            event_loop_handle: self.0.clone(),
            handler_instance,
            source,
            _callback: callback,
        })
    }

    fn internal_post<P>(&self, source: &event_bus::Source<P>, payload: &P) -> Result<(), EspError>
    where
        P: Copy,
    {
        let raw_source = as_raw_source_id(&source);

        // TODO: Handle the case where data size is < 4 as an optimization

        esp!(unsafe {
            esp_event_post(
                raw_source,
                0,
                payload as *const _ as *mut _,
                mem::size_of::<P>() as _,
                TickType_t::max_value(),
            )
        })
    }
}

impl<T> EspEventBus<User<T>> {
    fn internal_subscribe<P, E>(
        &self,
        source: event_bus::Source<P>,
        mut callback: impl for<'b> FnMut(&'b P) -> Result<(), E> + 'static,
    ) -> Result<EspSubscription<P, User<T>>, EspError>
    where
        E: Display + Debug + Send + Sync + 'static,
    {
        let raw_source = as_raw_source_id(&source);

        let mut handler_instance: esp_event_handler_instance_t = ptr::null_mut();

        let callback: Box<dyn FnMut(&P) + 'static> =
            Box::new(move |payload| callback(payload).unwrap());
        let mut callback = Box::new(callback);

        let unsafe_callback = UnsafeCallback(&mut *callback as *mut _);

        esp!(unsafe {
            esp_event_handler_instance_register_with(
                self.0 .0 .0,
                raw_source,
                ESP_EVENT_ANY_ID,
                Some(EspSubscription::<P, User<T>>::handle),
                &unsafe_callback as *const _ as *mut _,
                &mut handler_instance as *mut _,
            )
        })?;

        Ok(EspSubscription {
            event_loop_handle: self.0.clone(),
            handler_instance,
            source,
            _callback: callback,
        })
    }

    fn internal_post<P>(&self, source: &event_bus::Source<P>, payload: &P) -> Result<(), EspError>
    where
        P: Copy,
    {
        let raw_source = as_raw_source_id(&source);

        // TODO: Handle the case where data size is < 4 as an optimization

        esp!(unsafe {
            esp_event_post_to(
                self.0 .0 .0,
                raw_source,
                0,
                payload as *const _ as *mut _,
                mem::size_of::<P>() as _,
                TickType_t::max_value(),
            )
        })
    }
}

impl<T> event_bus::Spin for EspEventBus<User<T>> {
    fn spin(&self, duration: Option<Duration>) -> Result<(), EspError> {
        esp!(unsafe { esp_event_loop_run(self.0 .0 .0, TickType::from(duration).0,) })
    }
}

impl EspEventBus<User<Background>> {
    pub fn new(conf: &BackgroundConfiguration) -> Result<Self, EspError> {
        Ok(Self(Arc::new(EventBusHandle::<User<Background>>::new(
            conf,
        )?)))
    }
}

impl EspEventBus<User<Explicit>> {
    pub fn new(conf: &Configuration) -> Result<Self, EspError> {
        Ok(Self(Arc::new(EventBusHandle::<User<Explicit>>::new(conf)?)))
    }
}

impl EspEventBus<User<Pinned>> {
    pub fn new(conf: &Configuration) -> Result<Self, EspError> {
        Ok(Self(Arc::new(EventBusHandle::<User<Pinned>>::new(conf)?)))
    }

    pub fn postbox(&self) -> Result<EspPostbox, EspError> {
        EspPostbox::new(self)
    }
}

impl event_bus::Postbox for EspEventBus<System> {
    fn post<P>(&self, source: &event_bus::Source<P>, payload: &P) -> Result<(), Self::Error>
    where
        P: Copy,
    {
        self.internal_post(source, payload)
    }
}

impl<T> event_bus::Postbox for EspEventBus<User<T>> {
    fn post<P>(&self, source: &event_bus::Source<P>, payload: &P) -> Result<(), Self::Error>
    where
        P: Copy,
    {
        self.internal_post(source, payload)
    }
}

impl event_bus::EventBus for EspEventBus<System> {
    type Subscription<P> = EspSubscription<P, System>;

    fn subscribe<P, E>(
        &self,
        source: event_bus::Source<P>,
        callback: impl for<'b> FnMut(&'b P) -> Result<(), E> + Send + 'static,
    ) -> Result<Self::Subscription<P>, EspError>
    where
        E: Display + Debug + Send + Sync + 'static,
    {
        self.internal_subscribe(source, callback)
    }
}

impl event_bus::EventBus for EspEventBus<User<Background>> {
    type Subscription<P> = EspSubscription<P, User<Background>>;

    fn subscribe<P, E>(
        &self,
        source: event_bus::Source<P>,
        callback: impl for<'b> FnMut(&'b P) -> Result<(), E> + Send + 'static,
    ) -> Result<Self::Subscription<P>, EspError>
    where
        E: Display + Debug + Send + Sync + 'static,
    {
        self.internal_subscribe(source, callback)
    }
}

unsafe impl Send for EspEventBus<User<Background>> {}
unsafe impl Sync for EspEventBus<User<Background>> {}

impl event_bus::EventBus for EspEventBus<User<Explicit>> {
    type Subscription<P> = EspSubscription<P, User<Explicit>>;

    fn subscribe<P, E>(
        &self,
        source: event_bus::Source<P>,
        callback: impl for<'b> FnMut(&'b P) -> Result<(), E> + Send + 'static,
    ) -> Result<Self::Subscription<P>, EspError>
    where
        E: Display + Debug + Send + Sync + 'static,
    {
        self.internal_subscribe(source, callback)
    }
}

unsafe impl Send for EspEventBus<User<Explicit>> {}
unsafe impl Sync for EspEventBus<User<Explicit>> {}

#[derive(Clone)]
pub struct EspPostbox(EspEventBus<User<Pinned>>);

impl EspPostbox {
    fn new(event_loop: &EspEventBus<User<Pinned>>) -> Result<Self, EspError> {
        Ok(Self(event_loop.clone()))
    }
}

impl service::Service for EspPostbox {
    type Error = EspError;
}

impl event_bus::Postbox for EspPostbox {
    fn post<P>(&self, source: &event_bus::Source<P>, payload: &P) -> Result<(), Self::Error>
    where
        P: Copy,
    {
        self.0.internal_post(source, payload)
    }
}

unsafe impl Send for EspPostbox {}
unsafe impl Sync for EspPostbox {}

impl event_bus::PinnedEventBus for EspEventBus<User<Pinned>> {
    type Subscription<P> = EspSubscription<P, User<Pinned>>;

    fn subscribe<P, E>(
        &self,
        source: event_bus::Source<P>,
        callback: impl for<'b> FnMut(&'b P) -> Result<(), E> + 'static,
    ) -> Result<Self::Subscription<P>, EspError>
    where
        E: Display + Debug + Send + Sync + 'static,
    {
        self.internal_subscribe(source, callback)
    }
}

fn as_raw_source_id<P>(source: &event_bus::Source<P>) -> *const c_types::c_char {
    source.id() as *const _ as *const _
}
