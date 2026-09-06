package demo;

/**
 * Mentions HttpClient in javadoc for token-mode=all.
 */
public class Foo {
    // HttpClient in a line comment
    private final HttpClient client;
    private final CloseableHttpClient other;

    public Foo(HttpClient client) {
        this.client = client;
        String label = "HttpClient";
    }
}
