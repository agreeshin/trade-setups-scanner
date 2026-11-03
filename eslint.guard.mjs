// Design-system guard rules for apps/web.
// Kept separate from the main config so unit tests can lint fixtures
// without the type-aware project service.
export const webGuardConfig = [
  {
    files: ["apps/web/**/*.{ts,tsx}"],
    languageOptions: {
      parserOptions: {
        ecmaFeatures: { jsx: true },
      },
    },
    rules: {
      "no-restricted-syntax": [
        "error",
        {
          selector: "JSXAttribute[name.name='style']",
          message:
            "Inline style attributes are forbidden: use Tailwind utility classes or extend design tokens in apps/web/src/styles/global.css.",
        },
      ],
      "no-restricted-imports": [
        "error",
        {
          patterns: [
            {
              group: [
                "antd",
                "antd/*",
                "@mui/*",
                "@chakra-ui/*",
                "@mantine/*",
                "@headlessui/*",
                "react-bootstrap",
                "react-bootstrap/*",
                "reactstrap",
                "reactstrap/*",
                "@blueprintjs/*",
                "primereact",
                "@arco-design/*",
                "semi-ui",
                "semi-ui/*",
              ],
              message:
                "Third-party UI libraries are forbidden: build screens from the local shadcn/ui layer (apps/web/src/components/ui).",
            },
            {
              group: [
                "styled-components",
                "@emotion/*",
                "@stitches/*",
                "goober",
                "linaria",
                "vanilla-extract",
              ],
              message:
                "CSS-in-JS libraries are forbidden: styling goes through Tailwind CSS and the design tokens in apps/web/src/styles/global.css.",
            },
          ],
        },
      ],
    },
  },
];
