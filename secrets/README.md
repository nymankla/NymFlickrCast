# secrets

Everything in this folder except this README is ignored by git.

NymFlickrCast expects:

- `flickrkey.txt`: your Flickr API key. Get one at https://www.flickr.com/services/apps/create/.
  The app uses the first 32-character hex string in the file as the key, so the file can be just the key, or the key
  and secret as Flickr shows them (labels are fine). The secret isn't used, because the app only reads public albums.
