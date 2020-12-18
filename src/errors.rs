use error_chain::error_chain;

error_chain! {
    links {
        Kubernetes(kubernetes::Error, kubernetes::ErrorKind);
        HTTP(reqwest::Error, reqwest::ErrorKind);
    }
}
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


pub mod reqwest {
    use error_chain::error_chain;

    error_chain! {
        types {
        }

        foreign_links {
            Fmt(::std::fmt::Error);
            Io(::std::io::Error) #[cfg(unix)];
            Http(::reqwest::Error);
        }

        errors {
        }
    }
}
