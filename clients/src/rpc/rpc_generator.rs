#[macro_export]
macro_rules! generate_rpc {
    (
        $Trait:ident,
        $Client:ident,
        [
            $(
                (
                    $fn:ident,
                    [ $( $( ? )? $arg:ident : $ty:ty ),* $(,)? ],
                    $Resp:ty,
                    $Out:ty
                )
            ),* $(,)?
        ]
    ) => {
        #[::async_trait::async_trait]
        pub trait $Trait {
            $(
                async fn $fn(&self $(, $arg: $ty )*)
                    -> ::core::result::Result<$Out, $crate::rpc::ChiaRpcError>;
            )*
        }

        #[::async_trait::async_trait]
        impl $Trait for $Client {
            $(
                async fn $fn(&self $(, $arg: $ty )*)
                    -> ::core::result::Result<$Out, $crate::rpc::ChiaRpcError>
                {
                    #[allow(unused_mut)]
                    let mut request_body: ::serde_json::Map<String, ::serde_json::Value> =
                        ::serde_json::Map::new();
                    $(
                        let __v = ::serde_json::to_value(&$arg)
                            .map_err(|e| $crate::rpc::ChiaRpcError { error: Some(e.to_string()), success: false })?;
                        if !__v.is_null() {
                            request_body.insert(stringify!($arg).to_string(), __v);
                        }
                    )*
                    let __res: Result<$Resp, $crate::rpc::ChiaRpcError> =
                        $crate::rpc::post::<$Resp, ::std::hash::RandomState>(
                            &self.client,
                            &(self.url_function)(self.host.as_str(), self.port, stringify!($fn)),
                            &request_body,
                            &self.additional_headers,
                        ).await;
                    __res.map(Into::into)
                }
            )*
        }
    };
}
