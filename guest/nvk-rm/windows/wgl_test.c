/*
 * wgl_test: OpenGL through WGL on the current opengl32.dll (app-local Mesa
 * opengl32 + libgallium_wgl = Zink, or the system opengl32 with the adapter's
 * ICD). Prints the GL strings, checks a rendered frame with glReadPixels,
 * runs a GL 4.3 compute shader and checks its output, checks that a
 * persistent (non-coherent) buffer mapping reaches the GPU, then spins a
 * glxgears-like scene for N seconds with swap interval 0 and prints fps.
 *
 *   wgl_test.exe [seconds=5] [width=1280] [height=720]
 *
 * Exit code 0 when the readback, compute and persistent-map checks pass.
 */
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <GL/gl.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifndef GL_COMPUTE_SHADER
#define GL_COMPUTE_SHADER 0x91B9
#define GL_SHADER_STORAGE_BUFFER 0x90D2
#define GL_COMPILE_STATUS 0x8B81
#define GL_LINK_STATUS 0x8B82
#define GL_DYNAMIC_READ 0x88E9
#define GL_MAP_READ_BIT 0x0001
#define GL_SHADER_STORAGE_BARRIER_BIT 0x00002000
#define GL_BUFFER_UPDATE_BARRIER_BIT 0x00000200
#endif
#ifndef GL_MAP_PERSISTENT_BIT
#define GL_MAP_WRITE_BIT 0x0002
#define GL_MAP_PERSISTENT_BIT 0x0040
#define GL_CLIENT_MAPPED_BUFFER_BARRIER_BIT 0x00004000
#define GL_COPY_READ_BUFFER 0x8F36
#define GL_COPY_WRITE_BUFFER 0x8F37
#define GL_STATIC_DRAW 0x88E4
#endif

typedef char GLchar;
typedef ptrdiff_t GLsizeiptr;
typedef ptrdiff_t GLintptr;
typedef GLuint(APIENTRY *PFNCREATESHADER)(GLenum);
typedef void(APIENTRY *PFNSHADERSOURCE)(GLuint, GLsizei, const GLchar *const *, const GLint *);
typedef void(APIENTRY *PFNCOMPILESHADER)(GLuint);
typedef void(APIENTRY *PFNGETSHADERIV)(GLuint, GLenum, GLint *);
typedef void(APIENTRY *PFNGETSHADERINFOLOG)(GLuint, GLsizei, GLsizei *, GLchar *);
typedef GLuint(APIENTRY *PFNCREATEPROGRAM)(void);
typedef void(APIENTRY *PFNATTACHSHADER)(GLuint, GLuint);
typedef void(APIENTRY *PFNLINKPROGRAM)(GLuint);
typedef void(APIENTRY *PFNGETPROGRAMIV)(GLuint, GLenum, GLint *);
typedef void(APIENTRY *PFNUSEPROGRAM)(GLuint);
typedef void(APIENTRY *PFNGENBUFFERS)(GLsizei, GLuint *);
typedef void(APIENTRY *PFNBINDBUFFER)(GLenum, GLuint);
typedef void(APIENTRY *PFNBUFFERDATA)(GLenum, GLsizeiptr, const void *, GLenum);
typedef void(APIENTRY *PFNBINDBUFFERBASE)(GLenum, GLuint, GLuint);
typedef void(APIENTRY *PFNDISPATCHCOMPUTE)(GLuint, GLuint, GLuint);
typedef void(APIENTRY *PFNMEMORYBARRIER)(GLbitfield);
typedef void *(APIENTRY *PFNMAPBUFFERRANGE)(GLenum, GLintptr, GLsizeiptr, GLbitfield);
typedef GLboolean(APIENTRY *PFNUNMAPBUFFER)(GLenum);
typedef BOOL(WINAPI *PFNWGLSWAPINTERVALEXT)(int);
typedef void(APIENTRY *PFNBUFFERSTORAGE)(GLenum, GLsizeiptr, const void *, GLbitfield);
typedef void(APIENTRY *PFNCOPYBUFFERSUBDATA)(GLenum, GLenum, GLintptr, GLintptr, GLsizeiptr);
typedef void(APIENTRY *PFNGETBUFFERSUBDATA)(GLenum, GLintptr, GLsizeiptr, void *);

static LRESULT CALLBACK
wndproc(HWND w, UINT m, WPARAM wp, LPARAM lp)
{
   if (m == WM_CLOSE) {
      PostQuitMessage(0);
      return 0;
   }
   return DefWindowProcA(w, m, wp, lp);
}

static double
now_s(void)
{
   static LARGE_INTEGER f;
   LARGE_INTEGER c;
   if (!f.QuadPart)
      QueryPerformanceFrequency(&f);
   QueryPerformanceCounter(&c);
   return (double)c.QuadPart / (double)f.QuadPart;
}

/* One gear as in glxgears, immediate mode. */
static void
gear(float inner, float outer, float width, int teeth, float depth)
{
   const float r0 = inner, r1 = outer - depth / 2.0f, r2 = outer + depth / 2.0f;
   const float da = 2.0f * 3.14159265f / teeth / 4.0f;
   glShadeModel(GL_FLAT);
   glNormal3f(0, 0, 1);
   glBegin(GL_QUAD_STRIP);
   for (int i = 0; i <= teeth; i++) {
      float a = i * 2.0f * 3.14159265f / teeth;
      glVertex3f(r0 * cosf(a), r0 * sinf(a), width * 0.5f);
      glVertex3f(r1 * cosf(a), r1 * sinf(a), width * 0.5f);
      glVertex3f(r0 * cosf(a), r0 * sinf(a), width * 0.5f);
      glVertex3f(r1 * cosf(a + 3 * da), r1 * sinf(a + 3 * da), width * 0.5f);
   }
   glEnd();
   glBegin(GL_QUADS);
   for (int i = 0; i < teeth; i++) {
      float a = i * 2.0f * 3.14159265f / teeth;
      glVertex3f(r1 * cosf(a), r1 * sinf(a), width * 0.5f);
      glVertex3f(r2 * cosf(a + da), r2 * sinf(a + da), width * 0.5f);
      glVertex3f(r2 * cosf(a + 2 * da), r2 * sinf(a + 2 * da), width * 0.5f);
      glVertex3f(r1 * cosf(a + 3 * da), r1 * sinf(a + 3 * da), width * 0.5f);
   }
   glEnd();
   glNormal3f(0, 0, -1);
   glBegin(GL_QUAD_STRIP);
   for (int i = 0; i <= teeth; i++) {
      float a = i * 2.0f * 3.14159265f / teeth;
      glVertex3f(r1 * cosf(a), r1 * sinf(a), -width * 0.5f);
      glVertex3f(r0 * cosf(a), r0 * sinf(a), -width * 0.5f);
      glVertex3f(r1 * cosf(a + 3 * da), r1 * sinf(a + 3 * da), -width * 0.5f);
      glVertex3f(r0 * cosf(a), r0 * sinf(a), -width * 0.5f);
   }
   glEnd();
   glBegin(GL_QUAD_STRIP);
   for (int i = 0; i < teeth; i++) {
      float a = i * 2.0f * 3.14159265f / teeth;
      glVertex3f(r1 * cosf(a), r1 * sinf(a), width * 0.5f);
      glVertex3f(r1 * cosf(a), r1 * sinf(a), -width * 0.5f);
      float u = r2 * cosf(a + da) - r1 * cosf(a), v = r2 * sinf(a + da) - r1 * sinf(a);
      glNormal3f(v, -u, 0);
      glVertex3f(r2 * cosf(a + da), r2 * sinf(a + da), width * 0.5f);
      glVertex3f(r2 * cosf(a + da), r2 * sinf(a + da), -width * 0.5f);
      glNormal3f(cosf(a), sinf(a), 0);
      glVertex3f(r2 * cosf(a + 2 * da), r2 * sinf(a + 2 * da), width * 0.5f);
      glVertex3f(r2 * cosf(a + 2 * da), r2 * sinf(a + 2 * da), -width * 0.5f);
      u = r1 * cosf(a + 3 * da) - r2 * cosf(a + 2 * da);
      v = r1 * sinf(a + 3 * da) - r2 * sinf(a + 2 * da);
      glNormal3f(v, -u, 0);
      glVertex3f(r1 * cosf(a + 3 * da), r1 * sinf(a + 3 * da), width * 0.5f);
      glVertex3f(r1 * cosf(a + 3 * da), r1 * sinf(a + 3 * da), -width * 0.5f);
      glNormal3f(cosf(a), sinf(a), 0);
   }
   glVertex3f(r1, 0, width * 0.5f);
   glVertex3f(r1, 0, -width * 0.5f);
   glEnd();
}

/* A persistent, non-coherent write mapping (GL 4.4 buffer storage): what the
 * CPU writes must reach the GPU after glMemoryBarrier(CLIENT_MAPPED_BUFFER),
 * without unmapping.  The GPU copies it to a second buffer, which is read
 * back.  A driver that backs such a mapping with a staging copy (Zink on a
 * device without host-visible VRAM did) returns zeros here. */
static int
persistent_map_check(void)
{
#define GP(t, n) t n = (t)(void *)wglGetProcAddress(#n)
   GP(PFNGENBUFFERS, glGenBuffers);
   GP(PFNBINDBUFFER, glBindBuffer);
   GP(PFNBUFFERDATA, glBufferData);
   GP(PFNBUFFERSTORAGE, glBufferStorage);
   GP(PFNCOPYBUFFERSUBDATA, glCopyBufferSubData);
   GP(PFNGETBUFFERSUBDATA, glGetBufferSubData);
   GP(PFNMEMORYBARRIER, glMemoryBarrier);
   GP(PFNMAPBUFFERRANGE, glMapBufferRange);
   GP(PFNUNMAPBUFFER, glUnmapBuffer);
#undef GP
   if (!glBufferStorage || !glCopyBufferSubData || !glGetBufferSubData || !glMemoryBarrier) {
      printf("persistent map: GL 4.4 entry points missing\n");
      return 1;
   }
   enum { N = 4096 };
   GLuint b[2];
   glGenBuffers(2, b);
   glBindBuffer(GL_COPY_READ_BUFFER, b[0]);
   glBufferStorage(GL_COPY_READ_BUFFER, N * 4, NULL, GL_MAP_WRITE_BIT | GL_MAP_PERSISTENT_BIT);
   uint32_t *p = glMapBufferRange(GL_COPY_READ_BUFFER, 0, N * 4,
                                  GL_MAP_WRITE_BIT | GL_MAP_PERSISTENT_BIT);
   if (!p) {
      printf("persistent map: map failed (0x%x)\n", glGetError());
      return 1;
   }
   for (unsigned i = 0; i < N; i++)
      p[i] = i * 2654435761u + 1;
   glMemoryBarrier(GL_CLIENT_MAPPED_BUFFER_BARRIER_BIT);
   glBindBuffer(GL_COPY_WRITE_BUFFER, b[1]);
   glBufferData(GL_COPY_WRITE_BUFFER, N * 4, NULL, GL_STATIC_DRAW);
   glCopyBufferSubData(GL_COPY_READ_BUFFER, GL_COPY_WRITE_BUFFER, 0, 0, N * 4);
   static uint32_t out[N];
   glGetBufferSubData(GL_COPY_WRITE_BUFFER, 0, N * 4, out);
   unsigned bad = 0;
   for (unsigned i = 0; i < N; i++)
      bad += out[i] != i * 2654435761u + 1;
   glUnmapBuffer(GL_COPY_READ_BUFFER);
   printf("persistent map: %u/%u values correct -> %s\n", N - bad, N, bad ? "FAIL" : "PASS");
   return bad != 0;
}

static int
compute_check(void)
{
#define GP(t, n) t n = (t)(void *)wglGetProcAddress(#n)
   GP(PFNCREATESHADER, glCreateShader);
   GP(PFNSHADERSOURCE, glShaderSource);
   GP(PFNCOMPILESHADER, glCompileShader);
   GP(PFNGETSHADERIV, glGetShaderiv);
   GP(PFNGETSHADERINFOLOG, glGetShaderInfoLog);
   GP(PFNCREATEPROGRAM, glCreateProgram);
   GP(PFNATTACHSHADER, glAttachShader);
   GP(PFNLINKPROGRAM, glLinkProgram);
   GP(PFNGETPROGRAMIV, glGetProgramiv);
   GP(PFNUSEPROGRAM, glUseProgram);
   GP(PFNGENBUFFERS, glGenBuffers);
   GP(PFNBINDBUFFER, glBindBuffer);
   GP(PFNBUFFERDATA, glBufferData);
   GP(PFNBINDBUFFERBASE, glBindBufferBase);
   GP(PFNDISPATCHCOMPUTE, glDispatchCompute);
   GP(PFNMEMORYBARRIER, glMemoryBarrier);
   GP(PFNMAPBUFFERRANGE, glMapBufferRange);
   GP(PFNUNMAPBUFFER, glUnmapBuffer);
#undef GP
   if (!glCreateShader || !glDispatchCompute || !glMapBufferRange) {
      printf("compute: GL 4.3 entry points missing\n");
      return 1;
   }
   static const char *src =
      "#version 430\n"
      "layout(local_size_x = 64) in;\n"
      "layout(std430, binding = 0) buffer B { uint v[]; };\n"
      "void main() { uint i = gl_GlobalInvocationID.x; v[i] = i * i + 7u; }\n";
   GLuint sh = glCreateShader(GL_COMPUTE_SHADER);
   glShaderSource(sh, 1, &src, NULL);
   glCompileShader(sh);
   GLint ok = 0;
   glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
   if (!ok) {
      char log[2048];
      glGetShaderInfoLog(sh, sizeof(log), NULL, log);
      printf("compute: compile failed: %s\n", log);
      return 1;
   }
   GLuint prog = glCreateProgram();
   glAttachShader(prog, sh);
   glLinkProgram(prog);
   glGetProgramiv(prog, GL_LINK_STATUS, &ok);
   if (!ok) {
      printf("compute: link failed\n");
      return 1;
   }
   const unsigned n = 64 * 1024;
   GLuint buf;
   glGenBuffers(1, &buf);
   glBindBuffer(GL_SHADER_STORAGE_BUFFER, buf);
   glBufferData(GL_SHADER_STORAGE_BUFFER, n * 4, NULL, GL_DYNAMIC_READ);
   glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, buf);
   glUseProgram(prog);
   glDispatchCompute(n / 64, 1, 1);
   glMemoryBarrier(GL_BUFFER_UPDATE_BARRIER_BIT | GL_SHADER_STORAGE_BARRIER_BIT);
   const uint32_t *p = glMapBufferRange(GL_SHADER_STORAGE_BUFFER, 0, n * 4, GL_MAP_READ_BIT);
   if (!p) {
      printf("compute: map failed (0x%x)\n", glGetError());
      return 1;
   }
   unsigned bad = 0;
   for (unsigned i = 0; i < n; i++)
      bad += p[i] != i * i + 7u;
   glUnmapBuffer(GL_SHADER_STORAGE_BUFFER);
   glUseProgram(0);
   printf("compute: %u/%u values correct -> %s\n", n - bad, n, bad ? "FAIL" : "PASS");
   return bad ? 1 : 0;
}

int
main(int argc, char **argv)
{
   const double seconds = argc > 1 ? atof(argv[1]) : 5.0;
   const int width = argc > 2 ? atoi(argv[2]) : 1280;
   const int height = argc > 3 ? atoi(argv[3]) : 720;
   setvbuf(stdout, NULL, _IONBF, 0);

   WNDCLASSA wc = {0};
   wc.style = CS_OWNDC;
   wc.lpfnWndProc = wndproc;
   wc.hInstance = GetModuleHandleA(NULL);
   wc.lpszClassName = "wgl_test";
   RegisterClassA(&wc);
   RECT r = {0, 0, width, height};
   AdjustWindowRect(&r, WS_OVERLAPPEDWINDOW, FALSE);
   HWND hwnd = CreateWindowA("wgl_test", "wgl_test (Zink)", WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                             40, 40, r.right - r.left, r.bottom - r.top, NULL, NULL,
                             wc.hInstance, NULL);
   HDC dc = GetDC(hwnd);
   PIXELFORMATDESCRIPTOR pfd = {sizeof(pfd), 1};
   pfd.dwFlags = PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER;
   pfd.iPixelType = PFD_TYPE_RGBA;
   pfd.cColorBits = 32;
   pfd.cDepthBits = 24;
   int pf = ChoosePixelFormat(dc, &pfd);
   if (!pf || !SetPixelFormat(dc, pf, &pfd)) {
      printf("pixel format failed (%lu)\n", GetLastError());
      return 2;
   }
   double t0 = now_s();
   HGLRC ctx = wglCreateContext(dc);
   if (!ctx || !wglMakeCurrent(dc, ctx)) {
      printf("wglCreateContext/MakeCurrent failed (%lu)\n", GetLastError());
      return 2;
   }
   printf("context in %.0f ms\n", (now_s() - t0) * 1000.0);
   printf("GL_VENDOR   %s\n", glGetString(GL_VENDOR));
   printf("GL_RENDERER %s\n", glGetString(GL_RENDERER));
   printf("GL_VERSION  %s\n", glGetString(GL_VERSION));

   /* 1. readback: clear blue-ish, red quad over the centre */
   int fail = 0;
   glViewport(0, 0, width, height);
   glClearColor(0.2f, 0.4f, 0.6f, 1.0f);
   glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
   glMatrixMode(GL_PROJECTION);
   glLoadIdentity();
   glMatrixMode(GL_MODELVIEW);
   glLoadIdentity();
   glColor3f(1, 0, 0);
   glBegin(GL_QUADS);
   glVertex2f(-0.5f, -0.5f);
   glVertex2f(0.5f, -0.5f);
   glVertex2f(0.5f, 0.5f);
   glVertex2f(-0.5f, 0.5f);
   glEnd();
   glFinish();
   unsigned char c[4], e[4];
   glReadPixels(width / 2, height / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, c);
   glReadPixels(2, 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, e);
   int ok = c[0] > 250 && c[1] < 5 && c[2] < 5 && abs(e[0] - 51) < 3 &&
            abs(e[1] - 102) < 3 && abs(e[2] - 153) < 3;
   printf("readback: centre %u,%u,%u edge %u,%u,%u -> %s\n", c[0], c[1], c[2], e[0], e[1],
          e[2], ok ? "PASS" : "FAIL");
   fail |= !ok;

   /* 2. compute */
   fail |= compute_check();

   /* 3. persistent mapping */
   fail |= persistent_map_check();

   /* 4. gears, swap interval 0 */
   PFNWGLSWAPINTERVALEXT swap_interval =
      (PFNWGLSWAPINTERVALEXT)(void *)wglGetProcAddress("wglSwapIntervalEXT");
   if (swap_interval)
      swap_interval(0);
   static const GLfloat pos[4] = {5, 5, 10, 0};
   static const GLfloat red[4] = {0.8f, 0.1f, 0, 1}, green[4] = {0, 0.8f, 0.2f, 1},
                        blue[4] = {0.2f, 0.2f, 1, 1};
   glLightfv(GL_LIGHT0, GL_POSITION, pos);
   glEnable(GL_CULL_FACE);
   glEnable(GL_LIGHTING);
   glEnable(GL_LIGHT0);
   glEnable(GL_DEPTH_TEST);
   GLuint g1 = glGenLists(3), g2 = g1 + 1, g3 = g1 + 2;
   glNewList(g1, GL_COMPILE);
   glMaterialfv(GL_FRONT, GL_AMBIENT_AND_DIFFUSE, red);
   gear(1, 4, 1, 20, 0.7f);
   glEndList();
   glNewList(g2, GL_COMPILE);
   glMaterialfv(GL_FRONT, GL_AMBIENT_AND_DIFFUSE, green);
   gear(0.5f, 2, 2, 10, 0.7f);
   glEndList();
   glNewList(g3, GL_COMPILE);
   glMaterialfv(GL_FRONT, GL_AMBIENT_AND_DIFFUSE, blue);
   gear(1.3f, 2, 0.5f, 10, 0.7f);
   glEndList();
   glEnable(GL_NORMALIZE);
   glMatrixMode(GL_PROJECTION);
   glLoadIdentity();
   const float h = (float)height / (float)width;
   glFrustum(-1, 1, -h, h, 5, 60);
   glMatrixMode(GL_MODELVIEW);
   glLoadIdentity();
   glTranslatef(0, 0, -40);

   unsigned frames = 0, total = 0;
   double start = now_s(), last = start, angle = 0;
   MSG msg;
   while (now_s() - start < seconds) {
      while (PeekMessageA(&msg, NULL, 0, 0, PM_REMOVE)) {
         TranslateMessage(&msg);
         DispatchMessageA(&msg);
      }
      angle += 1.0;
      glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
      glPushMatrix();
      glRotatef(20, 1, 0, 0);
      glRotatef(30, 0, 1, 0);
      glPushMatrix();
      glTranslatef(-3, -2, 0);
      glRotatef((float)angle, 0, 0, 1);
      glCallList(g1);
      glPopMatrix();
      glPushMatrix();
      glTranslatef(3.1f, -2, 0);
      glRotatef((float)(-2 * angle - 9), 0, 0, 1);
      glCallList(g2);
      glPopMatrix();
      glPushMatrix();
      glTranslatef(-3.1f, 4.2f, 0);
      glRotatef((float)(-2 * angle - 25), 0, 0, 1);
      glCallList(g3);
      glPopMatrix();
      glPopMatrix();
      SwapBuffers(dc);
      frames++;
      total++;
      double t = now_s();
      if (t - last >= 1.0) {
         printf("%u frames in %.1f s = %.1f fps\n", frames, t - last, frames / (t - last));
         frames = 0;
         last = t;
      }
   }
   double span = now_s() - start;
   printf("gears: %u frames in %.1f s = %.1f fps (%dx%d, swap interval 0)\n", total, span,
          total / span, width, height);
   GLenum err = glGetError();
   if (err)
      printf("glGetError 0x%x\n", err);

   wglMakeCurrent(NULL, NULL);
   wglDeleteContext(ctx);
   ReleaseDC(hwnd, dc);
   DestroyWindow(hwnd);
   printf("%s\n", fail ? "RESULT FAIL" : "RESULT PASS");
   return fail;
}
