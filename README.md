# Hook

https://hook.snd.one

This is a very simple webhook inspection service. Think https://webhook.site but with no setup and no web interface. For hook, all you need is `curl`.

# Live

This is hosted live at https://hook.snd.one.

# Usage

First, listen to a hook with `curl -L hook.snd.one/my/hook` (the path can be just about anything).

Now, any HTTP requests to the same path will be streamed to your console. So for example, you can open a new terminal tab and run `curl -L hook.snd.one -d hello=world` to see it in action.

For practical use, you'd want to "hook this up" to a real webhooks system, like Stripe or GitHub. Just create a new webhook pointing to `https://hook.snd.one/whatever` and you can listen on it with curl whenever you like.

Obviously the paths are "global". If you use an easily guessable path, then someone else might read your data.

Also, hook doesn't have any persistent storage. All data sent through is stored in memory for a short time and then dropped.
