pub mod kubernetes {
    use error_chain::error_chain;

    error_chain! {
        types {
        }

        foreign_links {
            Fmt(::std::fmt::Error);
            Io(::std::io::Error) #[cfg(unix)];
            Kube(::kube::Error);
        }

        errors {
            CommunicationError(t: String) {
                description("unable to communicate with k8s")
                display("unable to communicate with k8s because: '{}'", t)
            }
        }
    }
}
