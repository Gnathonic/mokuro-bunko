# JPEG XL page fixtures

Small JPEG XL files for the decoder tests, made with libjxl's `cjxl` 0.12.

- `page-rgb.jxl`: a 24×16 RGB image, lossless, as a bare codestream. Each pixel is
  R = 10·x, G = 15·y, and B = 200 when x + y is even, else 40.
- `page-container.jxl`: the same pixels inside the ISO BMFF container.
- `page-grey.jxl`: a 24×16 greyscale image, lossless, as a bare codestream, with
  pixel value 10·x + y.
- `page-from-jpeg.jxl`: `page.jpg` (a 64×48 white-to-black greyscale gradient)
  transcoded losslessly, which is how most JPEG XL comics are made.

The source images were written as PPM/PGM, then encoded with:

```sh
cjxl rgb.ppm page-rgb.jxl -d 0 -e 7 --container=0
cjxl rgb.ppm page-container.jxl -d 0 -e 7 --container=1
cjxl grey.pgm page-grey.jxl -d 0 -e 7 --container=0
magick -size 64x48 gradient:white-black -quality 90 page.jpg
cjxl page.jpg page-from-jpeg.jxl --lossless_jpeg=1
```

`djxl` decodes the three lossless files back to their source pixels exactly.
